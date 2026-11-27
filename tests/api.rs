mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::json;

use common::*;

#[tokio::test]
async fn setup_requires_token_and_runs_once() {
    let t = test_app().await;
    let status = t.get("/api/v1/auth/status", None).await.json();
    assert_eq!(status["setup_required"], true);
    assert_eq!(status["version"], forgenerd::maintenance::VERSION);

    let body = |token: &str| json!({"setup_token": token, "email": EMAIL, "password": PASSWORD});
    assert_eq!(
        t.post("/api/v1/auth/setup", body("wrong"), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let short = json!({"setup_token": SETUP_TOKEN, "email": EMAIL, "password": "short"});
    assert_eq!(
        t.post("/api/v1/auth/setup", short, None).await.status,
        StatusCode::BAD_REQUEST
    );

    let created = t.post("/api/v1/auth/setup", body(SETUP_TOKEN), None).await;
    assert_eq!(created.status, StatusCode::CREATED);
    assert!(created.json().get("password_hash").is_none());

    let auth = t.login().await;
    let settings = t.get("/api/v1/settings", Some(&auth)).await.json();
    assert_eq!(settings["panel_domain"], serde_json::Value::Null);
    assert_eq!(settings["acme_email"], EMAIL);

    assert_eq!(
        t.post("/api/v1/auth/setup", body(SETUP_TOKEN), None)
            .await
            .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        t.get("/api/v1/auth/status", None).await.json()["setup_required"],
        false
    );
}

#[tokio::test]
async fn login_session_and_logout() {
    let t = test_app().await;
    t.setup().await;

    let bad = t
        .post(
            "/api/v1/auth/login",
            json!({"email": EMAIL, "password": "nope-nope-nope"}),
            None,
        )
        .await;
    assert_eq!(bad.status, StatusCode::UNAUTHORIZED);

    let cookie = t.login().await;
    let me = t.get("/api/v1/auth/me", Some(&cookie)).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.json()["email"], EMAIL);

    assert_eq!(
        t.post("/api/v1/auth/logout", json!({}), Some(&cookie))
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        t.get("/api/v1/auth/me", Some(&cookie)).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn nodes_require_auth_and_list_local() {
    let t = test_app().await;
    t.setup().await;
    assert_eq!(
        t.get("/api/v1/nodes", None).await.status,
        StatusCode::UNAUTHORIZED
    );

    let cookie = t.login().await;
    let res = t.get("/api/v1/nodes", Some(&cookie)).await;
    assert_eq!(res.status, StatusCode::OK);
    let nodes = res.json();
    assert_eq!(nodes.as_array().unwrap().len(), 1);
    assert_eq!(nodes[0]["id"], "local");
}

#[tokio::test]
async fn api_tokens_work_until_deleted() {
    let t = test_app().await;
    t.setup().await;
    let cookie = t.login().await;

    let created = t
        .post("/api/v1/auth/tokens", json!({"name": "ci"}), Some(&cookie))
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let body = created.json();
    let bearer = format!("bearer:{}", body["token"].as_str().unwrap());
    let id = body["id"].as_str().unwrap().to_string();

    assert_eq!(
        t.get("/api/v1/nodes", Some(&bearer)).await.status,
        StatusCode::OK
    );
    let listed = t.get("/api/v1/auth/tokens", Some(&cookie)).await.json();
    assert!(listed[0].get("token").is_none());

    assert_eq!(
        t.delete(&format!("/api/v1/auth/tokens/{id}"), &cookie)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        t.get("/api/v1/nodes", Some(&bearer)).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn state_changing_requests_must_be_json() {
    let t = test_app().await;
    let req = Request::post("/api/v1/auth/login")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("email=a&password=b"))
        .unwrap();
    assert_eq!(t.send(req).await.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn spa_fallback_and_api_404() {
    let t = test_app().await;
    let page = t.get("/apps/some-client-route", None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );

    let missing = t.get("/api/v1/does-not-exist", None).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.json()["error"], "not found");
}

#[tokio::test]
async fn cleanup_schedule_is_validated_and_runs_on_demand() {
    let t = test_app().await;
    t.setup().await;
    let auth = t.login().await;

    let config = t
        .get("/api/v1/maintenance/cleanup", Some(&auth))
        .await
        .json();
    assert_eq!(config, json!({"cron_expr": null, "keep_images": 3}));

    for bad in [
        json!({"cron_expr": "every day", "keep_images": 3}),
        json!({"cron_expr": "0 1 * * *", "keep_images": 0}),
    ] {
        let res = t.patch("/api/v1/maintenance/cleanup", bad, &auth).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST);
    }
    let saved = t
        .patch(
            "/api/v1/maintenance/cleanup",
            json!({"cron_expr": " 0 1 * * * ", "keep_images": 5}),
            &auth,
        )
        .await;
    assert_eq!(saved.status, StatusCode::OK);
    assert_eq!(
        saved.json(),
        json!({"cron_expr": "0 1 * * *", "keep_images": 5})
    );

    let cleared = t
        .patch(
            "/api/v1/maintenance/cleanup",
            json!({"cron_expr": "", "keep_images": 5}),
            &auth,
        )
        .await
        .json();
    assert_eq!(cleared["cron_expr"], serde_json::Value::Null);

    let report = t
        .post("/api/v1/maintenance/cleanup/run", json!({}), Some(&auth))
        .await;
    assert_eq!(report.status, StatusCode::OK);
    assert_eq!(report.json()["errors"], json!([]));

    assert_eq!(
        t.get("/api/v1/maintenance/cleanup", None).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn logins_are_throttled_per_client() {
    let t = test_app().await;
    t.setup().await;
    let attempt = |password: &'static str| {
        let t = &t;
        async move {
            t.post(
                "/api/v1/auth/login",
                json!({"email": EMAIL, "password": password}),
                None,
            )
            .await
        }
    };
    for _ in 0..forgenerd::auth::throttle::MAX_FAILURES {
        assert_eq!(
            attempt("wrong-password").await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    // Even the right password is refused until the window passes.
    let res = attempt(PASSWORD).await;
    assert_eq!(res.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(res.headers.contains_key("retry-after"));
    t.state
        .login_throttle
        .reset(std::net::IpAddr::from([0, 0, 0, 0]));
    assert_eq!(attempt(PASSWORD).await.status, StatusCode::OK);
}

#[tokio::test]
async fn password_change_requires_current_and_signs_out_other_sessions() {
    let t = test_app().await;
    let auth = t.admin().await;
    let other = t.login().await;
    let change =
        |current: &str, new: &str| json!({"current_password": current, "new_password": new});
    let res = t
        .post(
            "/api/v1/auth/password",
            change("wrong-password", "another-long-password"),
            Some(&auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = t
        .post(
            "/api/v1/auth/password",
            change(PASSWORD, "short"),
            Some(&auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = t
        .post(
            "/api/v1/auth/password",
            change(PASSWORD, "another-long-password"),
            Some(&auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.text());

    assert_eq!(
        t.get("/api/v1/auth/me", Some(&auth)).await.status,
        StatusCode::OK
    );
    assert_eq!(
        t.get("/api/v1/auth/me", Some(&other)).await.status,
        StatusCode::UNAUTHORIZED
    );
    let old = t
        .post(
            "/api/v1/auth/login",
            json!({"email": EMAIL, "password": PASSWORD}),
            None,
        )
        .await;
    assert_eq!(old.status, StatusCode::UNAUTHORIZED);
    let new = t
        .post(
            "/api/v1/auth/login",
            json!({"email": EMAIL, "password": "another-long-password"}),
            None,
        )
        .await;
    assert_eq!(new.status, StatusCode::OK);
}

#[tokio::test]
async fn disk_usage_is_reported() {
    let t = test_app().await;
    let auth = t.admin().await;
    let res = t.get("/api/v1/maintenance/disk", Some(&auth)).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.json()["filesystem_free_bytes"], 4000);
    assert_eq!(
        t.get("/api/v1/maintenance/disk", None).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn panel_update_requests_go_through_the_updater_files() {
    let t = test_app().await;
    let auth = t.admin().await;
    let dir = t.state.data_dir.join("update");
    let install = |version: &str| json!({ "version": version });

    let anonymous = t
        .post(
            "/api/v1/maintenance/updates/install",
            install("99.0.0"),
            None,
        )
        .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    // Installs from before the updater existed have no update directory.
    let res = t
        .post(
            "/api/v1/maintenance/updates/install",
            install("99.0.0"),
            Some(&auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    std::fs::create_dir(&dir).unwrap();

    let idle = t
        .get("/api/v1/maintenance/updates/progress", Some(&auth))
        .await;
    assert_eq!(idle.json()["state"], "idle");

    for bad in [
        "",
        "latest",
        "1.0;rm -rf /",
        "v0.0.1",
        forgenerd::maintenance::VERSION,
    ] {
        let res = t
            .post(
                "/api/v1/maintenance/updates/install",
                install(bad),
                Some(&auth),
            )
            .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{bad:?}");
    }
    assert!(!dir.join("request").exists());

    let res = t
        .post(
            "/api/v1/maintenance/updates/install",
            install("99.0.0"),
            Some(&auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::ACCEPTED);
    assert_eq!(res.json()["state"], "pending");
    assert_eq!(res.json()["version"], "v99.0.0");
    assert_eq!(
        std::fs::read_to_string(dir.join("request")).unwrap(),
        "v99.0.0\n"
    );
    assert!(!dir.join("request.tmp").exists());

    let again = t
        .post(
            "/api/v1/maintenance/updates/install",
            install("v99.0.0"),
            Some(&auth),
        )
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);

    // The updater takes the request, then records the outcome.
    std::fs::remove_file(dir.join("request")).unwrap();
    let running = t
        .get("/api/v1/maintenance/updates/progress", Some(&auth))
        .await;
    assert_eq!(running.json()["state"], "running");
    let busy = t
        .post(
            "/api/v1/maintenance/updates/install",
            install("99.0.0"),
            Some(&auth),
        )
        .await;
    assert_eq!(busy.status, StatusCode::CONFLICT);

    std::fs::write(dir.join("log"), "downloading\nchecksum mismatch\n").unwrap();
    std::fs::write(dir.join("result"), "failed v99.0.0\n").unwrap();
    let failed = t
        .get("/api/v1/maintenance/updates/progress", Some(&auth))
        .await
        .json();
    assert_eq!(failed["state"], "failed");
    assert_eq!(failed["version"], "v99.0.0");
    assert_eq!(failed["log"], "downloading\nchecksum mismatch");

    // A failed update can be retried, which clears the old result.
    let retry = t
        .post(
            "/api/v1/maintenance/updates/install",
            install("99.0.0"),
            Some(&auth),
        )
        .await;
    assert_eq!(retry.status, StatusCode::ACCEPTED);
    assert!(!dir.join("result").exists());
    std::fs::remove_file(dir.join("request")).unwrap();
    std::fs::write(dir.join("result"), "ok v99.0.0\n").unwrap();
    let done = t
        .get("/api/v1/maintenance/updates/progress", Some(&auth))
        .await
        .json();
    assert_eq!(done["state"], "succeeded");
    assert_eq!(done["log"], "");
}
