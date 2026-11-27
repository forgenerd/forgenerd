mod common;

use axum::http::StatusCode;
use serde_json::{Value, json};

use common::*;
use forgenerd::deploy::reconciler;
use forgenerd::executor::{ContainerAction, ContainerHealth, ProxyRoute};

const STACK: &str = r#"
services:
  web:
    image: nginx:alpine
    ports: ["8080:80"]
    environment:
      DATABASE_URL: ${DATABASE_URL}
  db:
    image: postgres:18-alpine
    volumes: [pg:/var/lib/postgresql]
volumes:
  pg:
"#;

async fn project(t: &TestApp, auth: &str) -> String {
    t.post("/api/v1/projects", json!({"name": "stack"}), Some(auth))
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn create(t: &TestApp, auth: &str, body: Value) -> Resp {
    t.post("/api/v1/apps", body, Some(auth)).await
}

#[tokio::test]
async fn compose_apps_are_validated_on_create_and_update() {
    let t = test_app().await;
    let auth = t.admin().await;
    let pid = project(&t, &auth).await;

    let bind = "services:\n  a:\n    image: x\n    volumes: ['/:/host']\n";
    for (body, why) in [
        (
            json!({"project_id": pid, "name": "s", "deploy_type": "compose"}),
            "missing yaml",
        ),
        (
            json!({"project_id": pid, "name": "s", "deploy_type": "compose", "compose_yaml": "services:\n  a:\n    image: x\n    privileged: true\n"}),
            "privileged",
        ),
        (
            json!({"project_id": pid, "name": "s", "deploy_type": "compose", "compose_yaml": STACK, "compose_service": "nope"}),
            "unknown service",
        ),
        (
            json!({"project_id": pid, "name": "forgenerd-x", "deploy_type": "compose", "compose_yaml": STACK}),
            "reserved name",
        ),
    ] {
        let res = create(&t, &auth, body).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{why}: {}", res.text());
    }
    // Host bind mounts pass the raw check (they are only detectable after interpolation,
    // which needs Docker); the executor rejects them at deploy time.
    let res = create(
        &t,
        &auth,
        json!({"project_id": pid, "name": "binds", "deploy_type": "compose", "compose_yaml": bind}),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);

    let res = create(
        &t,
        &auth,
        json!({"project_id": pid, "name": "shop", "deploy_type": "compose", "compose_yaml": STACK, "compose_service": "web", "container_port": 80}),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.text());
    let app = res.json();
    assert_eq!(app["compose_service"], "web");
    let id = app["id"].as_str().unwrap();

    let patch = |body: Value| {
        let t = &t;
        let auth = auth.clone();
        let path = format!("/api/v1/apps/{id}");
        async move { t.patch(&path, body, &auth).await.status }
    };
    assert_eq!(
        patch(json!({"compose_yaml": "services:\n  web:\n    image: x\n    network_mode: host\n"}))
            .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        patch(json!({"compose_service": "db2"})).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        patch(json!({"volumes": [{"name": "v", "mount_path": "/v"}]})).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        patch(json!({"image_name": "nginx"})).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        patch(json!({"compose_service": null})).await,
        StatusCode::OK
    );
    assert_eq!(
        patch(json!({"compose_service": "web"})).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn compose_deploy_routes_through_exposed_service() {
    let t = test_app().await;
    let auth = t.admin().await;
    let pid = project(&t, &auth).await;
    let id = create(
        &t,
        &auth,
        json!({"project_id": pid, "name": "shop", "deploy_type": "compose", "compose_yaml": STACK, "compose_service": "web", "container_port": 80}),
    )
    .await
    .json()["id"]
    .as_str()
    .unwrap()
    .to_string();
    t.patch(
        &format!("/api/v1/apps/{id}"),
        json!({"env": {"DATABASE_URL": {"value": "postgres://x"}}, "domains": [{"hostname": "shop.example.com"}]}),
        &auth,
    )
    .await;

    let res = t
        .post(&format!("/api/v1/apps/{id}/deploy"), json!({}), Some(&auth))
        .await;
    assert_eq!(res.status, StatusCode::ACCEPTED);
    let dep = t
        .wait_deployment(res.json()["deployment_id"].as_str().unwrap(), &auth)
        .await;
    assert_eq!(dep["status"], "healthy", "{dep}");
    assert_eq!(
        dep["image_ref"],
        Value::Null,
        "compose deployments have no single image"
    );

    let spec = t.fake.composes.lock().unwrap()[0].clone();
    assert_eq!(spec.expose_service.as_deref(), Some("web"));
    assert_eq!(
        spec.env.get("DATABASE_URL").map(String::as_str),
        Some("postgres://x")
    );
    assert!(spec.compose_yaml.contains("postgres:18-alpine"));
    assert_eq!(
        t.fake.proxy().routes,
        vec![ProxyRoute {
            hostnames: vec!["shop.example.com".into()],
            upstream: "forgenerd-shop-web-1:80".into(),
            force_https: true
        }]
    );
    let app = t
        .get(&format!("/api/v1/apps/{id}"), Some(&auth))
        .await
        .json();
    assert_eq!(app["status"], "running");
    assert_eq!(app["active_container_id"], format!("compose-{id}-web"));

    // Rollback needs an image; compose deployments have none.
    let res = t
        .post(
            &format!(
                "/api/v1/deployments/{}/rollback",
                dep["id"].as_str().unwrap()
            ),
            json!({}),
            Some(&auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::CONFLICT);

    // Start/stop act on every container of the stack.
    let stack_container = |cid: &str| ContainerHealth {
        id: cid.into(),
        app_id: id.clone(),
        name: cid.into(),
        deployment_id: String::new(),
        running: true,
        restarting: false,
        restart_count: 0,
        health: None,
        ip: None,
    };
    *t.fake.containers.lock().unwrap() = vec![stack_container("c-web"), stack_container("c-db")];
    let res = t
        .post(
            &format!("/api/v1/apps/{id}/restart"),
            json!({}),
            Some(&auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK);
    let mut acted: Vec<String> = t
        .fake
        .actions
        .lock()
        .unwrap()
        .iter()
        .map(|(c, a)| {
            assert!(matches!(a, ContainerAction::Restart));
            c.clone()
        })
        .collect();
    acted.sort();
    assert_eq!(acted, vec!["c-db", "c-web"]);

    let res = t
        .delete(&format!("/api/v1/apps/{id}?remove_volumes=true"), &auth)
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert_eq!(
        *t.fake.composes_removed.lock().unwrap(),
        vec![("forgenerd-shop".to_string(), true)]
    );
    assert!(t.fake.proxy().routes.is_empty());
}

#[tokio::test]
async fn compose_services_show_every_service_and_control_one_container() {
    let t = test_app().await;
    let auth = t.admin().await;
    let pid = project(&t, &auth).await;
    let id = create(
        &t,
        &auth,
        json!({"project_id": pid, "name": "shop", "deploy_type": "compose", "compose_yaml": STACK, "compose_service": "web", "container_port": 80}),
    )
    .await
    .json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let deploy = t
        .post(&format!("/api/v1/apps/{id}/deploy"), json!({}), Some(&auth))
        .await;
    t.wait_deployment(deploy.json()["deployment_id"].as_str().unwrap(), &auth)
        .await;

    let container = |id: &str, name: &str| ContainerHealth {
        id: id.into(),
        app_id: id.to_string(),
        name: name.into(),
        deployment_id: String::new(),
        running: true,
        restarting: false,
        restart_count: 0,
        health: Some("healthy".into()),
        ip: None,
    };
    *t.fake.containers.lock().unwrap() = vec![
        ContainerHealth {
            app_id: id.clone(),
            ..container("c-web", "forgenerd-shop-web-1")
        },
        ContainerHealth {
            app_id: id.clone(),
            ..container("c-db", "forgenerd-shop-db-1")
        },
    ];

    let services = t
        .get(&format!("/api/v1/apps/{id}/services"), Some(&auth))
        .await;
    assert_eq!(services.status, StatusCode::OK, "{}", services.text());
    let services = services.json();
    assert_eq!(services.as_array().unwrap().len(), 2);
    let web = services
        .as_array()
        .unwrap()
        .iter()
        .find(|service| service["name"] == "web")
        .unwrap();
    assert_eq!(web["exposed"], true);
    assert_eq!(web["present"], true);
    assert_eq!(
        web["environment"],
        json!([{ "key": "DATABASE_URL", "value": "${DATABASE_URL}" }])
    );
    assert_eq!(web["ports"], json!(["8080:80"]));
    let db = services
        .as_array()
        .unwrap()
        .iter()
        .find(|service| service["name"] == "db")
        .unwrap();
    assert_eq!(db["exposed"], false);
    assert_eq!(db["volumes"], json!(["pg:/var/lib/postgresql"]));

    let restart = t
        .post(
            &format!("/api/v1/apps/{id}/services/db/restart"),
            json!({}),
            Some(&auth),
        )
        .await;
    assert_eq!(restart.status, StatusCode::OK, "{}", restart.text());
    assert_eq!(restart.json()["name"], "db");
    let actions = t.fake.actions.lock().unwrap().clone();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].0, "c-db");
    assert!(matches!(actions[0].1, ContainerAction::Restart));

    let unknown = t
        .post(
            &format!("/api/v1/apps/{id}/services/worker/restart"),
            json!({}),
            Some(&auth),
        )
        .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);

    let update = t
        .patch(
            &format!("/api/v1/apps/{id}/services/web"),
            json!({
                "environment": [
                    {"key": "DATABASE_URL", "value": "postgres://updated"},
                    {"key": "APP_MODE", "value": "production"}
                ]
            }),
            &auth,
        )
        .await;
    assert_eq!(update.status, StatusCode::OK, "{}", update.text());
    let updated = update.json();
    assert_eq!(updated["redeploy_required"], true);
    assert_eq!(updated["environment"][0]["key"], "APP_MODE");
    assert_eq!(updated["environment"][1]["value"], "postgres://updated");

    let settings = t
        .patch(
            &format!("/api/v1/apps/{id}/services/web"),
            json!({
                "ports": ["127.0.0.1:8081:80"],
                "volumes": ["pg:/var/cache"]
            }),
            &auth,
        )
        .await;
    assert_eq!(settings.status, StatusCode::OK, "{}", settings.text());
    let settings = settings.json();
    assert_eq!(settings["ports"], json!(["127.0.0.1:8081:80"]));
    assert_eq!(settings["volumes"], json!(["pg:/var/cache"]));
    assert_eq!(settings["environment"][0]["key"], "APP_MODE");

    let clear = t
        .patch(
            &format!("/api/v1/apps/{id}/services/web"),
            json!({"ports": [], "volumes": []}),
            &auth,
        )
        .await;
    assert_eq!(clear.status, StatusCode::OK, "{}", clear.text());
    let clear = clear.json();
    assert_eq!(clear["ports"], json!([]));
    assert_eq!(clear["volumes"], json!([]));

    let duplicate = t
        .patch(
            &format!("/api/v1/apps/{id}/services/web"),
            json!({
                "environment": [
                    {"key": "APP_MODE", "value": "one"},
                    {"key": "APP_MODE", "value": "two"}
                ]
            }),
            &auth,
        )
        .await;
    assert_eq!(duplicate.status, StatusCode::BAD_REQUEST);

    // Resource limits are written into the compose file and read back.
    let limits = t
        .patch(
            &format!("/api/v1/apps/{id}/services/db"),
            json!({"memory_limit_mb": 512, "cpu_limit": 1.5}),
            &auth,
        )
        .await;
    assert_eq!(limits.status, StatusCode::OK, "{}", limits.text());
    assert_eq!(limits.json()["memory_limit_mb"], 512);
    assert_eq!(limits.json()["cpu_limit"], 1.5);
    let yaml = t
        .get(&format!("/api/v1/apps/{id}/compose"), Some(&auth))
        .await
        .text();
    assert!(
        yaml.contains("mem_limit: 512m") && yaml.contains("cpus: 1.5"),
        "{yaml}"
    );
    let bad = t
        .patch(
            &format!("/api/v1/apps/{id}/services/db"),
            json!({"memory_limit_mb": 1}),
            &auth,
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let cleared = t
        .patch(
            &format!("/api/v1/apps/{id}/services/db"),
            json!({"memory_limit_mb": null, "cpu_limit": null}),
            &auth,
        )
        .await
        .json();
    assert_eq!(cleared["memory_limit_mb"], Value::Null);
    assert_eq!(cleared["cpu_limit"], Value::Null);
    let services = t
        .get(&format!("/api/v1/apps/{id}/services"), Some(&auth))
        .await
        .json();
    assert_eq!(services[1]["name"], "web");
    assert_eq!(services[1]["memory_limit_mb"], Value::Null);
}

#[tokio::test]
async fn blueprints_create_and_deploy_databases() {
    let t = test_app().await;
    let auth = t.admin().await;
    let pid = project(&t, &auth).await;

    let list = t.get("/api/v1/blueprints", Some(&auth)).await.json()["blueprints"].clone();
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    // Sorted by category (Databases, Automation, Admin tools), then by name.
    assert_eq!(ids, ["mysql", "postgres", "redis", "n8n", "mongo-express"]);
    let postgres = list
        .as_array()
        .unwrap()
        .iter()
        .find(|blueprint| blueprint["id"] == "postgres")
        .unwrap();
    assert_eq!(postgres["image"], "postgres");
    assert_eq!(postgres["image_tag"], "18-alpine");
    assert_eq!(
        postgres["parameters"][0]["key"], "POSTGRES_USER",
        "blueprint parameters keep YAML order"
    );
    assert_eq!(
        postgres["parameters"][0]["description"],
        "PostgreSQL application user"
    );
    assert!(
        postgres["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|parameter| {
                parameter["key"] == "POSTGRES_PASSWORD"
                    && parameter["default_value"].is_null()
                    && parameter["generate"] == true
            })
    );

    assert_eq!(
        t.post(
            "/api/v1/blueprints/nope/deploy",
            json!({"project_id": pid, "name": "x"}),
            Some(&auth)
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    let res = t
        .post(
            "/api/v1/blueprints/postgres/deploy",
            json!({"project_id": pid, "name": "main-db"}),
            Some(&auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.text());
    let body = res.json();
    assert_eq!(body["internal_host"], "main-db");
    assert_eq!(body["app"]["exposed"], false, "databases stay internal");
    let url = body["connection_string"].as_str().unwrap().to_string();
    assert!(
        url.starts_with("postgres://app:") && url.ends_with("@main-db:5432/app"),
        "{url}"
    );

    let dep = t
        .wait_deployment(body["deployment_id"].as_str().unwrap(), &auth)
        .await;
    assert_eq!(dep["status"], "healthy");
    let spec = t.fake.composes.lock().unwrap()[0].clone();
    let password = spec.env.get("POSTGRES_PASSWORD").unwrap();
    assert!(url.contains(password.as_str()));
    assert_eq!(spec.expose_service.as_deref(), Some("postgres"));

    // The generated password is never returned again.
    let app = t
        .get(
            &format!("/api/v1/apps/{}", body["app"]["id"].as_str().unwrap()),
            Some(&auth),
        )
        .await;
    assert!(!app.text().contains(password.as_str()));
    let env = app.json()["env"].clone();
    let pw = env
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["key"] == "POSTGRES_PASSWORD")
        .unwrap();
    assert_eq!(pw["value"], Value::Null);

    let res = t
        .post(
            "/api/v1/blueprints/mysql/deploy",
            json!({
                "project_id": pid,
                "name": "custom-mysql",
                "image_tag": "8.4.33",
                "env": {
                    "MYSQL_USER": "reporting",
                    "MYSQL_DATABASE": "analytics",
                    "MYSQL_PASSWORD": "app-password",
                    "MYSQL_ROOT_PASSWORD": "root-password"
                }
            }),
            Some(&auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.text());
    let custom = res.json();
    assert_eq!(
        custom["connection_string"],
        "mysql://reporting:app-password@custom-mysql:3306/analytics"
    );
    t.wait_deployment(custom["deployment_id"].as_str().unwrap(), &auth)
        .await;
    let mysql = t.fake.composes.lock().unwrap().last().unwrap().clone();
    assert!(mysql.compose_yaml.contains("image: mysql:8.4.33"));
    assert_eq!(
        mysql.env.get("MYSQL_USER").map(String::as_str),
        Some("reporting")
    );
    assert_eq!(
        mysql.env.get("MYSQL_ROOT_PASSWORD").map(String::as_str),
        Some("root-password")
    );

    // App names are unique across blueprints too.
    let res = t
        .post(
            "/api/v1/blueprints/redis/deploy",
            json!({"project_id": pid, "name": "main-db"}),
            Some(&auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn reconciler_watches_whole_compose_stacks() {
    let t = test_app().await;
    let auth = t.admin().await;
    let pid = project(&t, &auth).await;
    let res = t
        .post(
            "/api/v1/blueprints/redis/deploy",
            json!({"project_id": pid, "name": "cache"}),
            Some(&auth),
        )
        .await
        .json();
    t.wait_deployment(res["deployment_id"].as_str().unwrap(), &auth)
        .await;
    let app_id = res["app"]["id"].as_str().unwrap().to_string();
    let c = |id: &str, app: &str, restarting: bool| ContainerHealth {
        id: id.into(),
        app_id: app.into(),
        name: id.into(),
        deployment_id: String::new(),
        running: !restarting,
        restarting,
        restart_count: 0,
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

    *t.fake.containers.lock().unwrap() =
        vec![c("redis-1", &app_id, false), c("sidecar", &app_id, false)];
    reconciler::reconcile_node(&t.state, "local", &mut memory)
        .await
        .unwrap();
    assert_eq!(status().await, "running");

    *t.fake.containers.lock().unwrap() =
        vec![c("redis-1", &app_id, false), c("sidecar", &app_id, true)];
    reconciler::reconcile_node(&t.state, "local", &mut memory)
        .await
        .unwrap();
    assert_eq!(
        status().await,
        "unhealthy",
        "one bad container makes the stack unhealthy"
    );

    // A stack whose containers are all gone is brought up again.
    let composes = t.fake.composes.lock().unwrap().len();
    t.fake.containers.lock().unwrap().clear();
    reconciler::reconcile_node(&t.state, "local", &mut memory)
        .await
        .unwrap();
    let ids = t.deployment_ids(&app_id).await;
    assert_eq!(ids.len(), 2);
    let recovery = t.wait_deployment(&ids[1], &auth).await;
    assert_eq!(recovery["status"], "healthy");
    assert_eq!(t.fake.composes.lock().unwrap().len(), composes + 1);
    assert_eq!(status().await, "running");

    // Stack containers of a live app are kept; those of a deleted app are orphans.
    *t.fake.containers.lock().unwrap() = vec![
        c("redis-1", &app_id, false),
        c("stale", "deleted-app", false),
    ];
    assert_eq!(reconciler::remove_orphans(&t.state).await.unwrap(), 1);
    assert_eq!(t.fake.removed(), vec!["stale".to_string()]);
}

#[tokio::test]
async fn web_app_blueprints_are_public_and_get_their_url() {
    let t = test_app().await;
    let auth = t.admin().await;
    let pid = project(&t, &auth).await;
    let set = t
        .patch(
            "/api/v1/settings",
            json!({"root_domain": "example.com"}),
            &auth,
        )
        .await;
    assert_eq!(set.status, StatusCode::OK, "{}", set.text());

    let list = t.get("/api/v1/blueprints", Some(&auth)).await.json()["blueprints"].clone();
    let n8n = list
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == "n8n")
        .unwrap();
    assert_eq!(n8n["category"], "Automation");
    assert_eq!(n8n["expose"], true);
    let url_param = n8n["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["key"] == "N8N_URL")
        .unwrap();
    assert_eq!(url_param["public_url"], true);
    assert!(
        url_param["default_value"].is_null(),
        "the placeholder is not shown"
    );

    let res = t
        .post(
            "/api/v1/blueprints/n8n/deploy",
            json!({"project_id": pid, "name": "flows"}),
            Some(&auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.text());
    let body = res.json();
    assert_eq!(body["app"]["exposed"], true);
    assert_eq!(body["url"], "https://flows.example.com");
    assert!(body["connection_string"].is_null());
    let generated = body["generated"].as_array().unwrap();
    assert_eq!(generated.len(), 1);
    assert_eq!(generated[0]["key"], "N8N_ENCRYPTION_KEY");
    let key = generated[0]["value"].as_str().unwrap().to_string();
    t.wait_deployment(body["deployment_id"].as_str().unwrap(), &auth)
        .await;
    let spec = t.fake.composes.lock().unwrap().last().unwrap().clone();
    assert_eq!(
        spec.env.get("N8N_URL").map(String::as_str),
        Some("https://flows.example.com")
    );
    assert_eq!(spec.env.get("N8N_ENCRYPTION_KEY"), Some(&key));

    // A parameter with no default must be supplied.
    let missing = t
        .post(
            "/api/v1/blueprints/mongo-express/deploy",
            json!({"project_id": pid, "name": "mongo-ui"}),
            Some(&auth),
        )
        .await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);
    assert!(
        missing.text().contains("MONGO_URL is required"),
        "{}",
        missing.text()
    );
    let ok = t
        .post(
            "/api/v1/blueprints/mongo-express/deploy",
            json!({"project_id": pid, "name": "mongo-ui", "env": {"MONGO_URL": "mongodb://admin:pw@mongodb:27017/"}}),
            Some(&auth),
        )
        .await;
    assert_eq!(ok.status, StatusCode::ACCEPTED, "{}", ok.text());
}
