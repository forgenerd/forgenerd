mod common;

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

use common::*;
use forgenerd::deploy::reconciler;
use forgenerd::executor::{BuildMethod, BuildSpec, ContainerAction, ContainerHealth};

/// The test project, created on first use.
async fn project(t: &TestApp, auth: &str) -> String {
    let existing = t.get("/api/v1/projects", Some(auth)).await.json();
    if let Some(id) = existing[0]["id"].as_str() {
        return id.to_string();
    }
    t.post("/api/v1/projects", json!({"name": "p"}), Some(auth))
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn git_app(t: &TestApp, auth: &str) -> String {
    let pid = project(t, auth).await;
    let res = t
        .post(
            "/api/v1/apps",
            json!({
                "project_id": pid, "name": "api", "deploy_type": "git",
                "git_repo_url": "https://github.com/acme/api.git", "git_branch": "prod",
                "git_credential": "ghp_token", "container_port": 3000
            }),
            Some(auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.text());
    let body = res.json();
    assert!(body.get("git_credential").is_none() && body.get("git_credential_enc").is_none());
    body["id"].as_str().unwrap().to_string()
}

async fn start_deploy(t: &TestApp, auth: &str, app_id: &str) -> String {
    let res = t
        .post(
            &format!("/api/v1/apps/{app_id}/deploy"),
            json!({}),
            Some(auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.text());
    res.json()["deployment_id"].as_str().unwrap().to_string()
}

async fn deploy(t: &TestApp, auth: &str, app_id: &str) -> Value {
    let id = start_deploy(t, auth, app_id).await;
    t.wait_deployment(&id, auth).await
}

#[tokio::test]
async fn git_apps_validate_and_build() {
    let t = test_app().await;
    let auth = t.admin().await;
    let pid = project(&t, &auth).await;
    for (body, why) in [
        (
            json!({"project_id": pid, "name": "a", "deploy_type": "git"}),
            "missing repo",
        ),
        (
            json!({"project_id": pid, "name": "a", "deploy_type": "git", "git_repo_url": "file:///etc"}),
            "file url",
        ),
        (
            json!({"project_id": pid, "name": "a", "deploy_type": "git", "git_repo_url": "https://x.io/a/b", "git_branch": "a..b"}),
            "bad branch",
        ),
        (
            json!({"project_id": pid, "name": "a", "deploy_type": "git", "git_repo_url": "https://x.io/a/b", "build_method": "make"}),
            "bad method",
        ),
    ] {
        assert_eq!(
            t.post("/api/v1/apps", body, Some(&auth)).await.status,
            StatusCode::BAD_REQUEST,
            "{why}"
        );
    }

    let app_id = git_app(&t, &auth).await;
    let dep = deploy(&t, &auth, &app_id).await;
    assert_eq!(dep["status"], "healthy");
    assert_eq!(dep["commit_sha"], "abc123");
    assert_eq!(
        dep["image_ref"],
        format!("forgenerd/api:{}", dep["id"].as_str().unwrap())
    );

    let builds = t.fake.builds.lock().unwrap().clone();
    match &builds[0] {
        BuildSpec::Git {
            repo_url,
            branch,
            credential,
            method,
            ..
        } => {
            assert_eq!(repo_url, "https://github.com/acme/api.git");
            assert_eq!(branch, "prod");
            assert_eq!(
                credential.as_deref(),
                Some("ghp_token"),
                "credential decrypted for the build"
            );
            assert_eq!(*method, BuildMethod::Dockerfile);
        }
        other => panic!("expected git build, got {other:?}"),
    }
    let pruned = t.fake.pruned.lock().unwrap().clone();
    assert_eq!(
        pruned,
        vec![(
            "api".to_string(),
            vec![dep["image_ref"].as_str().unwrap().to_string()]
        )]
    );

    // Git settings can change; image settings cannot be applied to git apps.
    let res = t
        .patch(
            &format!("/api/v1/apps/{app_id}"),
            json!({"git_branch": "main", "build_method": "railpack", "git_credential": null}),
            &auth,
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.text());
    assert_eq!(res.json()["redeploy_required"], true);
    assert_eq!(
        t.patch(
            &format!("/api/v1/apps/{app_id}"),
            json!({"image_name": "nginx"}),
            &auth
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    deploy(&t, &auth, &app_id).await;
    match &t.fake.builds.lock().unwrap()[1] {
        BuildSpec::Git {
            branch,
            credential,
            method,
            ..
        } => {
            assert_eq!(branch, "main");
            assert_eq!(*credential, None);
            assert_eq!(*method, BuildMethod::Railpack);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn rollback_reuses_previous_image() {
    let t = test_app().await;
    let auth = t.admin().await;
    let app_id = git_app(&t, &auth).await;
    let first = deploy(&t, &auth, &app_id).await;
    let second = deploy(&t, &auth, &app_id).await;
    let first_id = first["id"].as_str().unwrap();
    let second_id = second["id"].as_str().unwrap();

    let live = t
        .post(
            &format!("/api/v1/deployments/{second_id}/rollback"),
            json!({}),
            Some(&auth),
        )
        .await;
    assert_eq!(live.status, StatusCode::CONFLICT, "already live");

    let res = t
        .post(
            &format!("/api/v1/deployments/{first_id}/rollback"),
            json!({}),
            Some(&auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.text());
    let rb_id = res.json()["deployment_id"].as_str().unwrap().to_string();
    let rb = t.wait_deployment(&rb_id, &auth).await;
    assert_eq!(rb["status"], "healthy");
    assert_eq!(rb["image_ref"], first["image_ref"]);
    assert_eq!(rb["commit_sha"], "abc123");
    assert_eq!(
        t.fake.builds.lock().unwrap().len(),
        2,
        "rollback does not rebuild"
    );

    let replaced = t
        .get(&format!("/api/v1/deployments/{second_id}"), Some(&auth))
        .await
        .json();
    assert_eq!(replaced["status"], "rolled_back");
    let app = t
        .get(&format!("/api/v1/apps/{app_id}"), Some(&auth))
        .await
        .json();
    assert_eq!(app["active_deployment_id"], rb_id);
    let log = t
        .get(&format!("/api/v1/deployments/{rb_id}/log"), Some(&auth))
        .await
        .text();
    assert!(log.contains("rolling back to image"), "{log}");

    // Failed deployments are not rollback targets.
    t.fake.set_unhealthy(true);
    let failed = deploy(&t, &auth, &app_id).await;
    let res = t
        .post(
            &format!(
                "/api/v1/deployments/{}/rollback",
                failed["id"].as_str().unwrap()
            ),
            json!({}),
            Some(&auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn queued_builds_collapse() {
    let t = test_app().await;
    let auth = t.admin().await;
    let app_id = git_app(&t, &auth).await;

    let hold = t.fake.hold_builds.lock().await;
    let running = start_deploy(&t, &auth, &app_id).await;
    // Wait until the first deployment holds the app lock and is building.
    for _ in 0..100 {
        let d = t
            .get(&format!("/api/v1/deployments/{running}"), Some(&auth))
            .await
            .json();
        if d["status"] == "building" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let queued = start_deploy(&t, &auth, &app_id).await;
    let again = start_deploy(&t, &auth, &app_id).await;
    assert_ne!(running, queued);
    assert_eq!(queued, again, "a second request joins the queued build");
    drop(hold);

    assert_eq!(
        t.wait_deployment(&running, &auth).await["status"],
        "healthy"
    );
    assert_eq!(t.wait_deployment(&queued, &auth).await["status"], "healthy");
    assert_eq!(t.fake.builds.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn container_actions_wait_for_running_deployments() {
    let t = test_app().await;
    let auth = t.admin().await;
    let app_id = git_app(&t, &auth).await;
    deploy(&t, &auth, &app_id).await;

    let hold = t.fake.hold_builds.lock().await;
    let pending = start_deploy(&t, &auth, &app_id).await;
    let stop = t
        .post(
            &format!("/api/v1/apps/{app_id}/stop"),
            json!({}),
            Some(&auth),
        )
        .await;
    assert_eq!(stop.status, StatusCode::CONFLICT, "{}", stop.text());
    assert!(stop.text().contains("deployment is in progress"));
    assert!(t.fake.actions.lock().unwrap().is_empty());
    drop(hold);

    assert_eq!(
        t.wait_deployment(&pending, &auth).await["status"],
        "healthy"
    );
    let stop = t
        .post(
            &format!("/api/v1/apps/{app_id}/stop"),
            json!({}),
            Some(&auth),
        )
        .await;
    assert_eq!(stop.status, StatusCode::OK);
    assert_eq!(stop.json()["status"], "stopped");
}

fn sign(secret: &str, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

async fn webhook(t: &TestApp, app_id: &str, headers: &[(&str, &str)], body: &str) -> Resp {
    let mut req = axum::http::Request::post(format!("/webhooks/git/{app_id}"))
        .header("content-type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    t.send(req.body(axum::body::Body::from(body.to_string())).unwrap())
        .await
}

#[tokio::test]
async fn git_webhooks_verify_and_filter_branch() {
    let t = test_app().await;
    let auth = t.admin().await;
    let app_id = git_app(&t, &auth).await;
    let info = t
        .get(&format!("/api/v1/apps/{app_id}/webhook"), Some(&auth))
        .await
        .json();
    assert_eq!(info["path"], format!("/webhooks/git/{app_id}"));
    let secret = info["secret"].as_str().unwrap().to_string();

    let push = r#"{"ref":"refs/heads/prod","after":"abc"}"#;
    assert_eq!(
        webhook(
            &t,
            &app_id,
            &[("x-hub-signature-256", &sign("wrong", push))],
            push
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        webhook(&t, &app_id, &[], push).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        webhook(
            &t,
            "no-such-app",
            &[("x-hub-signature-256", &sign(&secret, push))],
            push
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );

    let ping = r#"{"zen":"hi"}"#;
    let res = webhook(
        &t,
        &app_id,
        &[
            ("x-hub-signature-256", &sign(&secret, ping)),
            ("x-github-event", "ping"),
        ],
        ping,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    let other = r#"{"ref":"refs/heads/main"}"#;
    let res = webhook(
        &t,
        &app_id,
        &[("x-hub-signature-256", &sign(&secret, other))],
        other,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.json()["ignored"].is_string());
    assert!(t.fake.builds.lock().unwrap().is_empty());

    let res = webhook(
        &t,
        &app_id,
        &[
            ("x-hub-signature-256", &sign(&secret, push)),
            ("x-github-event", "push"),
        ],
        push,
    )
    .await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.text());
    let dep_id = res.json()["deployment_id"].as_str().unwrap().to_string();
    assert_eq!(t.wait_deployment(&dep_id, &auth).await["status"], "healthy");

    let res = webhook(
        &t,
        &app_id,
        &[("x-gitlab-token", &secret), ("x-gitlab-event", "Push Hook")],
        push,
    )
    .await;
    assert_eq!(res.status, StatusCode::ACCEPTED);

    // Image apps have no webhook.
    let pid = t.get("/api/v1/projects", Some(&auth)).await.json()[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let img = t
        .post(
            "/api/v1/apps",
            json!({"project_id": pid, "name": "img", "image_name": "nginx"}),
            Some(&auth),
        )
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        t.get(&format!("/api/v1/apps/{img}/webhook"), Some(&auth))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        webhook(&t, &img, &[("x-gitlab-token", &secret)], push)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

/// Local HTTP endpoint that records notification payloads.
async fn notification_sink() -> (String, Arc<Mutex<Vec<Value>>>) {
    let received = Arc::new(Mutex::new(Vec::new()));
    let store = received.clone();
    let app = axum::Router::new().route(
        "/hook/secret-path",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let store = store.clone();
            async move {
                store.lock().unwrap().push(body);
                StatusCode::NO_CONTENT
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook/secret-path", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, received)
}

async fn wait_for(received: &Arc<Mutex<Vec<Value>>>, n: usize) -> Vec<Value> {
    for _ in 0..200 {
        if received.lock().unwrap().len() >= n {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    received.lock().unwrap().clone()
}

#[tokio::test]
async fn telegram_target_validates_credentials_and_hides_destination() {
    let t = test_app().await;
    let auth = t.admin().await;
    let path = "/api/v1/notifications";
    for (token, chat, expected) in [
        ("123:test", "-100234", StatusCode::CREATED),
        ("123:test", "@channel_name", StatusCode::CREATED),
        ("123:test", "123&other=value", StatusCode::BAD_REQUEST),
        ("123:test/path", "123", StatusCode::BAD_REQUEST),
    ] {
        let response = t
            .post(
                path,
                json!({"kind":"telegram", "bot_token":token, "chat_id":chat}),
                Some(&auth),
            )
            .await;
        assert_eq!(response.status, expected);
    }
    let targets = t.get(path, Some(&auth)).await.json();
    assert_eq!(targets.as_array().unwrap().len(), 2);
    for target in targets.as_array().unwrap() {
        assert_eq!(target["kind"], "telegram");
        assert_eq!(target["host"], "api.telegram.org");
    }
    for secret in ["123:test", "-100234", "@channel_name"] {
        assert!(!targets.to_string().contains(secret));
    }
}

#[tokio::test]
async fn reconciler_tracks_health_and_notifies() {
    let t = test_app().await;
    let auth = t.admin().await;
    let (url, received) = notification_sink().await;

    assert_eq!(
        t.post(
            "/api/v1/notifications",
            json!({"kind": "sms", "url": url}),
            Some(&auth)
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        t.post(
            "/api/v1/notifications",
            json!({"kind": "generic", "url": url}),
            Some(&auth)
        )
        .await
        .status,
        StatusCode::CREATED
    );
    let targets = t.get("/api/v1/notifications", Some(&auth)).await.json();
    assert!(
        !targets.to_string().contains("secret-path"),
        "URL paths (secrets) are not returned"
    );
    let target_id = targets[0]["id"].as_str().unwrap();
    assert_eq!(
        t.post(
            &format!("/api/v1/notifications/{target_id}/test"),
            json!({}),
            Some(&auth)
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(wait_for(&received, 1).await.len(), 1);

    let app_id = git_app(&t, &auth).await;
    let dep = deploy(&t, &auth, &app_id).await;
    let dep_id = dep["id"].as_str().unwrap().to_string();
    let container = |running: bool, restarts: i64| ContainerHealth {
        id: fake_container_id(&dep_id),
        app_id: app_id.clone(),
        name: fake_container_id(&dep_id),
        deployment_id: dep_id.clone(),
        running,
        restarting: false,
        restart_count: restarts,
        health: None,
        ip: None,
    };
    let status = || async {
        t.get(&format!("/api/v1/apps/{app_id}"), Some(&auth))
            .await
            .json()["status"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let mut memory = reconciler::Memory::default();

    *t.fake.containers.lock().unwrap() = vec![container(true, 0)];
    reconciler::reconcile_node(&t.state, "local", &mut memory)
        .await
        .unwrap();
    assert_eq!(status().await, "running");

    *t.fake.containers.lock().unwrap() = vec![container(true, 2)];
    reconciler::reconcile_node(&t.state, "local", &mut memory)
        .await
        .unwrap();
    assert_eq!(status().await, "unhealthy");
    let events = wait_for(&received, 2).await;
    assert_eq!(events[1]["to"], "unhealthy");
    assert!(events[1]["detail"].as_str().unwrap().contains("crash loop"));

    reconciler::reconcile_node(&t.state, "local", &mut memory)
        .await
        .unwrap();
    assert_eq!(status().await, "running", "stable restart count: recovered");

    // A vanished container is recreated from the active deployment's image.
    let before = t.deployment_ids(&app_id).await;
    let builds = t.fake.builds.lock().unwrap().len();
    t.fake.containers.lock().unwrap().clear();
    reconciler::reconcile_node(&t.state, "local", &mut memory)
        .await
        .unwrap();
    let events = wait_for(&received, 4).await;
    assert_eq!(events[3]["to"], "failed");
    assert!(events[3]["detail"].as_str().unwrap().contains("recreating"));
    let after = t.deployment_ids(&app_id).await;
    assert_eq!(after.len(), before.len() + 1);
    let recovery = t.wait_deployment(after.last().unwrap(), &auth).await;
    assert_eq!(recovery["status"], "healthy");
    assert_eq!(recovery["image_ref"], dep["image_ref"]);
    assert_eq!(t.fake.builds.lock().unwrap().len(), builds, "no rebuild");
    assert_eq!(status().await, "running");
    let events = wait_for(&received, 5).await;
    assert_eq!(events[4]["from"], "failed");
    assert_eq!(events[4]["to"], "running");
    assert!(events[4]["detail"].as_str().unwrap().contains("recreated"));
    let original = t
        .get(&format!("/api/v1/deployments/{dep_id}"), Some(&auth))
        .await
        .json();
    assert_eq!(
        original["status"], "healthy",
        "a recovery is not a rollback"
    );

    // Stopped apps are left alone.
    t.post(
        &format!("/api/v1/apps/{app_id}/stop"),
        json!({}),
        Some(&auth),
    )
    .await;
    *t.fake.containers.lock().unwrap() = vec![container(false, 2)];
    reconciler::reconcile_node(&t.state, "local", &mut memory)
        .await
        .unwrap();
    assert_eq!(status().await, "stopped");
}

async fn tick(t: &TestApp, memory: &mut reconciler::Memory) {
    reconciler::reconcile_node(&t.state, "local", memory)
        .await
        .unwrap();
}

async fn app_status(t: &TestApp, auth: &str, app_id: &str) -> String {
    t.get(&format!("/api/v1/apps/{app_id}"), Some(auth))
        .await
        .json()["status"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn self_healing_restarts_hung_and_stopped_containers() {
    let t = test_app().await;
    let auth = t.admin().await;
    let app_id = git_app(&t, &auth).await;
    let dep_id = deploy(&t, &auth, &app_id).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let cid = fake_container_id(&dep_id);
    let container = |running: bool| ContainerHealth {
        id: cid.clone(),
        app_id: app_id.clone(),
        name: cid.clone(),
        deployment_id: dep_id.clone(),
        running,
        restarting: false,
        restart_count: 0,
        health: None,
        ip: Some("10.0.0.2".into()),
    };
    let actions = || t.fake.actions.lock().unwrap().clone();
    let mut memory = reconciler::Memory::default();

    // Running but not answering HTTP: a blip is tolerated, a persistent failure restarts it.
    *t.fake.containers.lock().unwrap() = vec![container(true)];
    t.fake.set_unhealthy(true);
    for _ in 0..2 {
        tick(&t, &mut memory).await;
        assert_eq!(app_status(&t, &auth, &app_id).await, "running");
        assert!(actions().is_empty());
    }
    tick(&t, &mut memory).await;
    assert_eq!(app_status(&t, &auth, &app_id).await, "unhealthy");
    assert!(matches!(&actions()[..], [(id, ContainerAction::Restart)] if *id == cid));

    t.fake.set_unhealthy(false);
    tick(&t, &mut memory).await;
    assert_eq!(app_status(&t, &auth, &app_id).await, "running");

    // Stopped outside forgenerd: started again.
    *t.fake.containers.lock().unwrap() = vec![container(false)];
    tick(&t, &mut memory).await;
    assert_eq!(app_status(&t, &auth, &app_id).await, "unhealthy");
    assert!(matches!(actions().last(), Some((id, ContainerAction::Start)) if *id == cid));

    // Docker's restart backoff is a crash loop, not a stop: left to Docker.
    let mut looping = container(false);
    looping.restarting = true;
    *t.fake.containers.lock().unwrap() = vec![looping];
    let before = actions().len();
    tick(&t, &mut memory).await;
    assert_eq!(actions().len(), before);

    // A Stop waits while self-healing holds the app's container lock, and healing
    // leaves the app alone once it is stopped.
    let lock = t.state.container_locks.get(&app_id);
    let guard = lock.lock().await;
    let stop = {
        let (app, path, cookie) = (
            t.app.clone(),
            format!("/api/v1/apps/{app_id}/stop"),
            auth.clone(),
        );
        tokio::spawn(async move {
            let (name, value) = auth_header(&cookie);
            let req = axum::http::Request::post(path)
                .header(name, value)
                .header("content-type", "application/json")
                .body(axum::body::Body::from("{}"))
                .unwrap();
            tower::ServiceExt::oneshot(app, req).await.unwrap().status()
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!stop.is_finished(), "stop waits for the container lock");
    drop(guard);
    assert_eq!(stop.await.unwrap(), StatusCode::OK);
    *t.fake.containers.lock().unwrap() = vec![container(false)];
    let before = actions().len();
    tick(&t, &mut memory).await;
    assert_eq!(actions().len(), before);
    assert_eq!(app_status(&t, &auth, &app_id).await, "stopped");
}

#[tokio::test]
async fn self_healing_gives_up_after_repeated_failures() {
    let t = test_app().await;
    let auth = t.admin().await;
    let (url, received) = notification_sink().await;
    t.post(
        "/api/v1/notifications",
        json!({"kind": "generic", "url": url}),
        Some(&auth),
    )
    .await;
    let app_id = git_app(&t, &auth).await;
    deploy(&t, &auth, &app_id).await;
    let mut memory = reconciler::Memory::default();

    // The container is gone and every recreation fails its health gate.
    t.fake.set_unhealthy(true);
    for attempt in 1..=5 {
        tick(&t, &mut memory).await;
        let ids = t.deployment_ids(&app_id).await;
        assert_eq!(ids.len(), 1 + attempt);
        let recovery = t.wait_deployment(ids.last().unwrap(), &auth).await;
        assert_eq!(recovery["status"], "failed");
        assert_eq!(app_status(&t, &auth, &app_id).await, "failed");
    }
    tick(&t, &mut memory).await;
    tick(&t, &mut memory).await;
    assert_eq!(t.deployment_ids(&app_id).await.len(), 6, "no more attempts");
    let events = wait_for(&received, 2).await;
    assert_eq!(
        events.len(),
        2,
        "one notification per state, not per attempt"
    );
    assert!(events[1]["detail"].as_str().unwrap().contains("gave up"));

    // A new deployment starts healing over.
    t.fake.set_unhealthy(false);
    deploy(&t, &auth, &app_id).await;
    tick(&t, &mut memory).await;
    assert_eq!(t.deployment_ids(&app_id).await.len(), 8);
}

#[tokio::test]
async fn orphaned_containers_are_removed_at_startup() {
    let t = test_app().await;
    let auth = t.admin().await;
    let app_id = git_app(&t, &auth).await;
    let dep = deploy(&t, &auth, &app_id).await;
    let live = dep["id"].as_str().unwrap().to_string();
    let c = |dep: &str| ContainerHealth {
        id: fake_container_id(dep),
        app_id: app_id.clone(),
        name: fake_container_id(dep),
        deployment_id: dep.to_string(),
        running: true,
        restarting: false,
        restart_count: 0,
        health: None,
        ip: None,
    };
    *t.fake.containers.lock().unwrap() = vec![c(&live), c("orphan-candidate")];
    let removed = reconciler::remove_orphans(&t.state).await.unwrap();
    assert_eq!(removed, 1);
    assert_eq!(
        t.fake.removed(),
        vec![fake_container_id("orphan-candidate")]
    );
}

#[tokio::test]
async fn deployment_logs_stream_over_websocket() {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

    let t = test_app().await;
    let auth = t.admin().await;
    let app_id = git_app(&t, &auth).await;
    let dep_id = deploy(&t, &auth, &app_id).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = t.app.clone();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cookie = auth.strip_prefix("cookie:").unwrap().to_string();
    let request = |origin: &str| {
        let mut req = format!("ws://{addr}/api/v1/deployments/{dep_id}/logs")
            .into_client_request()
            .unwrap();
        req.headers_mut().insert("cookie", cookie.parse().unwrap());
        req.headers_mut().insert("origin", origin.parse().unwrap());
        req
    };

    // Cross-site origin is refused.
    match tokio_tungstenite::connect_async(request("https://evil.example")).await {
        Err(tungstenite::Error::Http(res)) => assert_eq!(res.status(), 403),
        other => panic!("expected 403, got {:?}", other.map(|_| ())),
    }

    let (mut ws, _) = tokio_tungstenite::connect_async(request(&format!("http://{addr}")))
        .await
        .unwrap();
    let mut lines = Vec::new();
    while let Some(Ok(msg)) = ws.next().await {
        match msg {
            tungstenite::Message::Text(text) => lines.push(text.to_string()),
            tungstenite::Message::Close(_) => break,
            _ => {}
        }
    }
    let all = lines.join("\n");
    assert!(
        all.contains("fake clone https://github.com/acme/api.git"),
        "{all}"
    );
    assert!(all.contains("deployment healthy"), "{all}");
}

#[tokio::test]
async fn resource_limits_and_health_timeout_apply_to_new_containers() {
    let t = test_app().await;
    let auth = t.admin().await;
    let app = git_app(&t, &auth).await;
    for (body, why) in [
        (json!({"memory_limit_mb": 4}), "memory too small"),
        (json!({"cpu_limit": 0}), "cpu zero"),
        (json!({"health_timeout_secs": 1}), "timeout too short"),
    ] {
        let res = t.patch(&format!("/api/v1/apps/{app}"), body, &auth).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{why}: {}", res.text());
    }
    let res = t
        .patch(
            &format!("/api/v1/apps/{app}"),
            json!({"memory_limit_mb": 256, "cpu_limit": 0.5, "health_timeout_secs": 120}),
            &auth,
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.text());
    assert_eq!(res.json()["memory_limit_mb"], 256);
    assert_eq!(res.json()["cpu_limit"], 0.5);
    assert_eq!(res.json()["health_timeout_secs"], 120);

    assert_eq!(deploy(&t, &auth, &app).await["status"], "healthy");
    {
        let started = t.fake.started.lock().unwrap();
        let spec = started.last().unwrap();
        assert_eq!(spec.memory_limit_mb, Some(256));
        assert_eq!(spec.cpu_limit, Some(0.5));
    }
    let stats = t
        .get(&format!("/api/v1/apps/{app}/stats"), Some(&auth))
        .await;
    assert_eq!(stats.status, StatusCode::OK);
    assert_eq!(stats.json().as_array().unwrap().len(), 1);

    // `null` removes a limit; changing limits needs a redeploy.
    let res = t
        .patch(
            &format!("/api/v1/apps/{app}"),
            json!({"memory_limit_mb": null, "health_timeout_secs": null}),
            &auth,
        )
        .await;
    assert_eq!(res.json()["memory_limit_mb"], Value::Null);
    assert_eq!(res.json()["cpu_limit"], 0.5);
    assert_eq!(res.json()["redeploy_required"], true);
}
