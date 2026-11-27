mod common;

use axum::http::StatusCode;
use serde_json::{Value, json};

use common::*;
use forgenerd::executor::{ContainerAction, ProxyRoute, container_name};

async fn project(t: &TestApp, auth: &str) -> String {
    let res = t
        .post("/api/v1/projects", json!({"name": "shop"}), Some(auth))
        .await;
    assert_eq!(res.status, StatusCode::CREATED);
    res.json()["id"].as_str().unwrap().to_string()
}

async fn app(t: &TestApp, auth: &str, project_id: &str, name: &str) -> Value {
    let res = t
        .post(
            "/api/v1/apps",
            json!({"project_id": project_id, "name": name, "image_name": "traefik/whoami:v1.10", "container_port": 80}),
            Some(auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.text());
    res.json()
}

async fn deploy(t: &TestApp, auth: &str, app_id: &str) -> Value {
    let res = t
        .post(
            &format!("/api/v1/apps/{app_id}/deploy"),
            json!({}),
            Some(auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.text());
    let id = res.json()["deployment_id"].as_str().unwrap().to_string();
    t.wait_deployment(&id, auth).await
}

#[tokio::test]
async fn create_app_validates_input() {
    let t = test_app().await;
    let auth = t.admin().await;
    let pid = project(&t, &auth).await;

    let attempt = |body: Value| {
        let t = &t;
        let auth = &auth;
        async move { t.post("/api/v1/apps", body, Some(auth)).await.status }
    };
    assert_eq!(
        attempt(json!({"project_id": pid, "name": "Bad_Name", "image_name": "nginx"})).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        attempt(json!({"project_id": pid, "name": "web", "image_name": "nginx;rm -rf /"})).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        attempt(
            json!({"project_id": pid, "name": "web", "image_name": "nginx", "container_port": 0})
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        attempt(json!({"project_id": pid, "name": "web", "deploy_type": "git"})).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        attempt(json!({"project_id": "nope", "name": "web", "image_name": "nginx"})).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        attempt(json!({"project_id": pid, "name": "web", "image_name": "nginx"})).await,
        StatusCode::CREATED
    );
    assert_eq!(
        attempt(json!({"project_id": pid, "name": "web", "image_name": "nginx"})).await,
        StatusCode::CONFLICT
    );

    assert_eq!(
        t.get("/api/v1/apps", None).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        t.get("/api/v1/apps", Some(&auth))
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let draft = t
        .post(
            "/api/v1/apps",
            json!({
                "project_id": pid,
                "name": "draft-app",
                "volumes": [{"name": "data", "mount_path": "/data"}]
            }),
            Some(&auth),
        )
        .await;
    assert_eq!(draft.status, StatusCode::CREATED, "{}", draft.text());
    let draft = draft.json();
    assert_eq!(draft["deploy_type"], "draft");
    assert_eq!(draft["volumes"][0]["mount_path"], "/data");

    let configured = t
        .post(
            &format!("/api/v1/apps/{}/deployment", draft["id"].as_str().unwrap()),
            json!({"deploy_type": "image", "image_name": "nginx:1"}),
            Some(&auth),
        )
        .await;
    assert_eq!(configured.status, StatusCode::OK, "{}", configured.text());
    assert_eq!(configured.json()["deploy_type"], "image");
}

#[tokio::test]
async fn deploy_configures_container_and_routes() {
    let t = test_app().await;
    let auth = t.admin().await;
    let pid = project(&t, &auth).await;
    let app_id = app(&t, &auth, &pid, "whoami").await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let res = t
        .patch(
            &format!("/api/v1/apps/{app_id}"),
            json!({
                "env": {"API_KEY": {"value": "s3cret"}, "MODE": {"value": "prod", "is_secret": false}},
                "domains": [{"hostname": "Who.Example.com"}, {"hostname": "plain.example.com", "force_https": false}],
                "volumes": [{"name": "data", "mount_path": "/data"}]
            }),
            &auth,
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.text());
    let detail = res.json();
    assert_eq!(detail["redeploy_required"], false, "nothing deployed yet");
    assert_eq!(detail["env"][0]["key"], "API_KEY");
    assert_eq!(
        detail["env"][0]["value"],
        Value::Null,
        "secret values are never returned"
    );
    assert_eq!(detail["env"][1]["value"], "prod");
    assert_eq!(detail["domains"][1]["hostname"], "who.example.com");

    let dep = deploy(&t, &auth, &app_id).await;
    assert_eq!(dep["status"], "healthy");
    assert_eq!(dep["image_ref"], "traefik/whoami:v1.10");
    let dep_id = dep["id"].as_str().unwrap();

    let started = t.fake.started();
    assert_eq!(started.len(), 1);
    assert_eq!(
        started[0].env.get("API_KEY").map(String::as_str),
        Some("s3cret")
    );
    assert_eq!(started[0].env.get("MODE").map(String::as_str), Some("prod"));
    assert_eq!(started[0].volumes[0].mount_path, "/data");
    assert_eq!(started[0].container_port, 80);

    let upstream = format!("{}:80", container_name("whoami", dep_id));
    assert_eq!(
        t.fake.proxy().routes,
        vec![
            ProxyRoute {
                hostnames: vec!["plain.example.com".into()],
                upstream: upstream.clone(),
                force_https: false
            },
            ProxyRoute {
                hostnames: vec!["who.example.com".into()],
                upstream,
                force_https: true
            },
        ]
    );

    let app = t
        .get(&format!("/api/v1/apps/{app_id}"), Some(&auth))
        .await
        .json();
    assert_eq!(app["status"], "running");
    assert_eq!(app["active_container_id"], fake_container_id(dep_id));
    // The healthy candidate gets the app's stable internal hostname before the swap.
    assert_eq!(
        *t.fake.aliases.lock().unwrap(),
        vec![(fake_container_id(dep_id), "whoami".to_string())]
    );

    let log = t
        .get(&format!("/api/v1/deployments/{dep_id}/log"), Some(&auth))
        .await
        .text();
    assert!(log.contains("fake pull traefik/whoami:v1.10"), "{log}");
    assert!(log.contains("deployment healthy"), "{log}");

    // Config changes on a deployed app need a redeploy.
    let res = t
        .patch(
            &format!("/api/v1/apps/{app_id}"),
            json!({"env": {"MODE": null}}),
            &auth,
        )
        .await
        .json();
    assert_eq!(res["redeploy_required"], true);
    assert_eq!(res["env"].as_array().unwrap().len(), 1);

    // A second deploy swaps containers and removes the old one.
    let dep2 = deploy(&t, &auth, &app_id).await;
    assert_eq!(dep2["status"], "healthy");
    assert_eq!(t.fake.removed(), vec![fake_container_id(dep_id)]);
    assert!(!t.fake.started()[1].env.contains_key("MODE"));
}

#[tokio::test]
async fn failed_health_check_keeps_previous_version() {
    let t = test_app().await;
    let auth = t.admin().await;
    let pid = project(&t, &auth).await;
    let app_id = app(&t, &auth, &pid, "api").await["id"]
        .as_str()
        .unwrap()
        .to_string();
    t.patch(
        &format!("/api/v1/apps/{app_id}"),
        json!({"domains": [{"hostname": "api.example.com"}]}),
        &auth,
    )
    .await;

    // First deploy fails: nothing was ever serving.
    t.fake.set_unhealthy(true);
    let failed = deploy(&t, &auth, &app_id).await;
    assert_eq!(failed["status"], "failed");
    let first_failed = failed["id"].as_str().unwrap();
    assert_eq!(t.fake.removed(), vec![fake_container_id(first_failed)]);
    let app = t
        .get(&format!("/api/v1/apps/{app_id}"), Some(&auth))
        .await
        .json();
    assert_eq!(app["status"], "failed");
    assert!(t.fake.proxy().routes.is_empty());

    // A healthy deploy, then a failing one: the healthy version keeps serving.
    t.fake.set_unhealthy(false);
    let good = deploy(&t, &auth, &app_id).await;
    let good_id = good["id"].as_str().unwrap();
    let routes_before = t.fake.proxy();

    t.fake.set_unhealthy(true);
    let bad = deploy(&t, &auth, &app_id).await;
    assert_eq!(bad["status"], "failed");
    let app = t
        .get(&format!("/api/v1/apps/{app_id}"), Some(&auth))
        .await
        .json();
    assert_eq!(app["status"], "running");
    assert_eq!(app["active_deployment_id"], good_id);
    assert_eq!(t.fake.proxy(), routes_before);
    // Failing candidates never answer on the app's name.
    assert!(
        t.fake
            .aliases
            .lock()
            .unwrap()
            .iter()
            .all(|(c, _)| c == &fake_container_id(good_id)),
        "only the healthy deployment was aliased"
    );
    let log = t
        .get(
            &format!("/api/v1/deployments/{}/log", bad["id"].as_str().unwrap()),
            Some(&auth),
        )
        .await
        .text();
    assert!(log.contains("previous version keeps serving"), "{log}");
}

#[tokio::test]
async fn panel_domain_and_app_domains_are_exclusive() {
    let t = test_app().await;
    let auth = t.admin().await;
    let pid = project(&t, &auth).await;
    let app_id = app(&t, &auth, &pid, "web").await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let res = t
        .patch(
            "/api/v1/settings",
            json!({
                "root_domain": "example.com",
                "panel_domain": "panel.example.com",
                "acme_email": "ops@example.com"
            }),
            &auth,
        )
        .await;
    assert_eq!(
        res.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        res.text()
    );

    let res = t
        .patch(
            "/api/v1/settings",
            json!({"root_domain": "example.com"}),
            &auth,
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.text());
    let body = res.json();
    assert_eq!(body["panel_domain"], "hero.example.com");
    assert_eq!(body["acme_email"], EMAIL);
    let proxy = t.fake.proxy();
    assert_eq!(proxy.panel_hostname.as_deref(), Some("hero.example.com"));
    assert_eq!(proxy.acme_email.as_deref(), Some(EMAIL));

    let res = t
        .patch(
            &format!("/api/v1/apps/{app_id}"),
            json!({"domains": [{"hostname": "hero.example.com"}]}),
            &auth,
        )
        .await;
    assert_eq!(res.status, StatusCode::CONFLICT);

    let res = t
        .patch("/api/v1/settings", json!({"panel_domain": null}), &auth)
        .await;
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY);

    let res = t
        .patch(
            "/api/v1/settings",
            json!({"root_domain": " Apps.Example.com. "}),
            &auth,
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.text());
    assert_eq!(res.json()["root_domain"], "apps.example.com");
    let res = t
        .patch("/api/v1/settings", json!({"root_domain": "nodot"}), &auth)
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        t.fake.proxy().panel_hostname.as_deref(),
        Some("hero.apps.example.com")
    );

    let res = t
        .patch(
            "/api/v1/settings",
            json!({"acme_email": "not-an-email"}),
            &auth,
        )
        .await;
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY);

    let res = t
        .patch("/api/v1/settings", json!({"root_domain": null}), &auth)
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.json()["panel_domain"], Value::Null);
    assert_eq!(t.fake.proxy().panel_hostname, None);
    assert_eq!(t.fake.proxy().acme_email.as_deref(), Some(EMAIL));
}

#[tokio::test]
async fn control_and_delete_app() {
    let t = test_app().await;
    let auth = t.admin().await;
    let pid = project(&t, &auth).await;
    let app_id = app(&t, &auth, &pid, "db").await["id"]
        .as_str()
        .unwrap()
        .to_string();
    t.patch(
        &format!("/api/v1/apps/{app_id}"),
        json!({"domains": [{"hostname": "db.example.com"}], "volumes": [{"name": "pgdata", "mount_path": "/var/lib/postgresql/data"}]}),
        &auth,
    )
    .await;

    // Nothing deployed yet.
    let res = t
        .post(
            &format!("/api/v1/apps/{app_id}/stop"),
            json!({}),
            Some(&auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::CONFLICT);

    let dep = deploy(&t, &auth, &app_id).await;
    let container = fake_container_id(dep["id"].as_str().unwrap());

    let res = t
        .post(
            &format!("/api/v1/apps/{app_id}/stop"),
            json!({}),
            Some(&auth),
        )
        .await;
    assert_eq!(res.json()["status"], "stopped");
    let res = t
        .post(
            &format!("/api/v1/apps/{app_id}/start"),
            json!({}),
            Some(&auth),
        )
        .await;
    assert_eq!(res.json()["status"], "running");
    assert_eq!(
        t.post(
            &format!("/api/v1/apps/{app_id}/explode"),
            json!({}),
            Some(&auth)
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    let actions = t.fake.actions.lock().unwrap().clone();
    assert!(
        matches!(actions.as_slice(), [(c1, ContainerAction::Stop), (c2, ContainerAction::Start)] if *c1 == container && *c2 == container)
    );

    // Projects with apps cannot be deleted.
    assert_eq!(
        t.delete(&format!("/api/v1/projects/{pid}"), &auth)
            .await
            .status,
        StatusCode::CONFLICT
    );

    let res = t
        .delete(&format!("/api/v1/apps/{app_id}?remove_volumes=true"), &auth)
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert_eq!(t.fake.removed(), vec![container]);
    assert!(t.fake.proxy().routes.is_empty());
    assert_eq!(
        *t.fake.removed_volumes.lock().unwrap(),
        vec!["forgenerd-db-pgdata".to_string()]
    );
    assert_eq!(
        t.get(&format!("/api/v1/apps/{app_id}"), Some(&auth))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        t.delete(&format!("/api/v1/projects/{pid}"), &auth)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn container_names_use_random_suffix() {
    // UUIDv7 heads are timestamps; names must differ for deploys in the same minute.
    let a = container_name("web", "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b");
    let b = container_name("web", "0190a1b2-c3d4-7e5f-8a9b-ffffffffffff");
    assert_eq!(a, "forgenerd-web-0c1d2e3f4a5b");
    assert_ne!(a, b);
}

#[tokio::test]
async fn certificate_status_is_probed_per_domain() {
    let t = test_app().await;
    let auth = t.admin().await;
    let pid = project(&t, &auth).await;
    let app_id = app(&t, &auth, &pid, "site").await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let empty = t
        .get(&format!("/api/v1/apps/{app_id}/certificates"), Some(&auth))
        .await;
    assert_eq!(empty.status, StatusCode::OK);
    assert_eq!(empty.json(), json!([]));

    t.patch(
        &format!("/api/v1/apps/{app_id}"),
        json!({"domains": [{"hostname": "site.example.com"}, {"hostname": "pending.example.com"}]}),
        &auth,
    )
    .await;
    let certs = t
        .get(&format!("/api/v1/apps/{app_id}/certificates"), Some(&auth))
        .await
        .json();
    assert_eq!(certs[0]["hostname"], "pending.example.com");
    assert_eq!(certs[0]["state"], "pending");
    assert_eq!(certs[1]["hostname"], "site.example.com");
    assert_eq!(certs[1]["state"], "issued");
    assert_eq!(certs[1]["issuer"], "Let's Encrypt (R11)");
    assert_eq!(
        t.get(&format!("/api/v1/apps/{app_id}/certificates"), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn apps_are_exposed_on_the_root_domain_by_default() {
    let t = test_app().await;
    let auth = t.admin().await;
    let pid = project(&t, &auth).await;
    let web = app(&t, &auth, &pid, "web").await;
    assert_eq!(web["exposed"], true);
    assert_eq!(web["default_domain"], Value::Null, "no root domain yet");
    let web_id = web["id"].as_str().unwrap().to_string();
    let dep = deploy(&t, &auth, &web_id).await;
    assert!(
        t.fake.proxy().routes.is_empty(),
        "without a root domain nothing is exposed"
    );
    let upstream = format!("{}:80", container_name("web", dep["id"].as_str().unwrap()));
    let route = |host: &str, force_https: bool| ProxyRoute {
        hostnames: vec![host.into()],
        upstream: upstream.clone(),
        force_https,
    };

    // Setting the root domain exposes existing apps immediately.
    let res = t
        .patch(
            "/api/v1/settings",
            json!({"root_domain": "apps.test"}),
            &auth,
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.text());
    assert_eq!(t.fake.proxy().routes, vec![route("web.apps.test", true)]);
    let detail = t
        .get(&format!("/api/v1/apps/{web_id}"), Some(&auth))
        .await
        .json();
    assert_eq!(detail["default_domain"], "web.apps.test");
    let board = t.get("/api/v1/apps", Some(&auth)).await.json();
    assert_eq!(board[0]["domains"], json!(["web.apps.test"]));

    // Custom domains are served in addition to the default one.
    t.patch(
        &format!("/api/v1/apps/{web_id}"),
        json!({"domains": [{"hostname": "shop.example.com", "force_https": false}]}),
        &auth,
    )
    .await;
    assert_eq!(
        t.fake.proxy().routes,
        vec![
            route("shop.example.com", false),
            route("web.apps.test", true)
        ]
    );

    // Not exposed: no public route at all; the custom domain is kept for later.
    let res = t
        .patch(
            &format!("/api/v1/apps/{web_id}"),
            json!({"exposed": false}),
            &auth,
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.text());
    assert_eq!(res.json()["domains"][0]["hostname"], "shop.example.com");
    assert!(t.fake.proxy().routes.is_empty());
    let board = t.get("/api/v1/apps", Some(&auth)).await.json();
    assert_eq!(board[0]["domains"], json!([]));
    let certs = t
        .get(&format!("/api/v1/apps/{web_id}/certificates"), Some(&auth))
        .await
        .json();
    assert_eq!(certs, json!([]));
    t.patch(
        &format!("/api/v1/apps/{web_id}"),
        json!({"exposed": true}),
        &auth,
    )
    .await;
    assert_eq!(t.fake.proxy().routes.len(), 2);

    // Another app's default domain, or the panel's, cannot be claimed.
    let api_id = app(&t, &auth, &pid, "api").await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let res = t
        .patch(
            &format!("/api/v1/apps/{api_id}"),
            json!({"domains": [{"hostname": "web.apps.test"}]}),
            &auth,
        )
        .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.text());
    let res = t
        .post(
            "/api/v1/apps",
            json!({"project_id": pid, "name": "hero", "image_name": "nginx:1"}),
            Some(&auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.text());

    // Created unexposed: never routed, even with a root domain.
    let res = t
        .post(
            "/api/v1/apps",
            json!({"project_id": pid, "name": "worker", "image_name": "nginx:1", "exposed": false}),
            Some(&auth),
        )
        .await;
    assert_eq!(res.json()["exposed"], false);
}
