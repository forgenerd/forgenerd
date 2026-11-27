mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};

use common::*;
use forgenerd::backup;
use forgenerd::executor::BackupSource;

struct Fixture {
    t: TestApp,
    auth: String,
    target: String,
    pg_app: String,
    web_app: String,
}

async fn fixture() -> Fixture {
    let t = test_app().await;
    let auth = t.admin().await;
    let pid = t
        .post("/api/v1/projects", json!({"name": "p"}), Some(&auth))
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let target = t
        .post(
            "/api/v1/s3-targets",
            json!({"name": "minio", "endpoint": "http://127.0.0.1:9000", "bucket": "backups",
                   "region": "us-east-1", "access_key": "AKIDEXAMPLE", "secret_key": "super-secret"}),
            Some(&auth),
        )
        .await;
    assert_eq!(target.status, StatusCode::CREATED, "{}", target.text());
    assert!(!target.text().contains("super-secret") && !target.text().contains("AKIDEXAMPLE"));
    let target = target.json()["id"].as_str().unwrap().to_string();

    let pg = t
        .post(
            "/api/v1/blueprints/postgres/deploy",
            json!({"project_id": pid, "name": "main-db"}),
            Some(&auth),
        )
        .await
        .json();
    t.wait_deployment(pg["deployment_id"].as_str().unwrap(), &auth)
        .await;
    let web = t
        .post(
            "/api/v1/apps",
            json!({"project_id": pid, "name": "web", "image_name": "nginx", "container_port": 80}),
            Some(&auth),
        )
        .await
        .json();
    let web_app = web["id"].as_str().unwrap().to_string();
    t.patch(
        &format!("/api/v1/apps/{web_app}"),
        json!({"volumes": [{"name": "uploads", "mount_path": "/srv/uploads"}]}),
        &auth,
    )
    .await;
    Fixture {
        pg_app: pg["app"]["id"].as_str().unwrap().to_string(),
        web_app,
        target,
        auth,
        t,
    }
}

async fn wait_run(t: &TestApp, auth: &str, job: &str) -> Value {
    for _ in 0..200 {
        let runs = t
            .get(&format!("/api/v1/backups/{job}/runs"), Some(auth))
            .await
            .json();
        if let Some(run) = runs.get(0)
            && run["status"] != "running"
        {
            return run.clone();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("backup run did not finish");
}

#[tokio::test]
async fn s3_targets_validate_and_protect_secrets() {
    let f = fixture().await;
    let (t, auth) = (&f.t, &f.auth);
    for (body, why) in [
        (
            json!({"name": "x", "endpoint": "s3.amazonaws.com", "bucket": "b-ok", "region": "us-east-1", "access_key": "a", "secret_key": "s"}),
            "no scheme",
        ),
        (
            json!({"name": "x", "endpoint": "https://s3.example.com/path", "bucket": "b-ok", "region": "us-east-1", "access_key": "a", "secret_key": "s"}),
            "path",
        ),
        (
            json!({"name": "x", "endpoint": "https://s3.example.com", "bucket": "Bad_Bucket", "region": "us-east-1", "access_key": "a", "secret_key": "s"}),
            "bucket",
        ),
        (
            json!({"name": "x", "endpoint": "https://s3.example.com", "bucket": "b-ok", "region": "us east", "access_key": "a", "secret_key": "s"}),
            "region",
        ),
        (
            json!({"name": "x", "endpoint": "https://s3.example.com", "bucket": "b-ok", "region": "auto", "access_key": "", "secret_key": "s"}),
            "empty key",
        ),
    ] {
        assert_eq!(
            t.post("/api/v1/s3-targets", body, Some(auth)).await.status,
            StatusCode::BAD_REQUEST,
            "{why}"
        );
    }
    let list = t.get("/api/v1/s3-targets", Some(auth)).await;
    assert!(!list.text().contains("super-secret"));
    assert_eq!(list.json()[0]["endpoint"], "http://127.0.0.1:9000");
    assert_eq!(list.json()[0]["last_test_at"], Value::Null);

    // Nothing listens on the fixture endpoint: the test fails, and the failure is kept.
    let res = t
        .post(
            &format!("/api/v1/s3-targets/{}/test", f.target),
            json!({}),
            Some(auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let target = t.get("/api/v1/s3-targets", Some(auth)).await.json()[0].clone();
    assert_eq!(target["last_test_ok"], false);
    assert!(target["last_test_at"].is_string());
    assert!(
        target["last_test_error"]
            .as_str()
            .unwrap()
            .contains("S3 request failed")
    );
}

#[tokio::test]
async fn backup_jobs_resolve_sources_and_record_runs() {
    let f = fixture().await;
    let (t, auth) = (&f.t, &f.auth);
    let create = |body: Value| async move { t.post("/api/v1/backups", body, Some(auth)).await };

    for (body, why) in [
        (
            json!({"app_id": f.pg_app, "s3_target_id": f.target, "driver": "postgres", "cron_expr": "0 2 * *"}),
            "cron fields",
        ),
        (
            json!({"app_id": f.pg_app, "s3_target_id": f.target, "driver": "postgres", "cron_expr": "0 2 * * *", "retention_count": 0}),
            "retention",
        ),
        (
            json!({"app_id": f.pg_app, "s3_target_id": f.target, "driver": "redis", "cron_expr": "0 2 * * *"}),
            "driver",
        ),
        (
            json!({"app_id": f.pg_app, "s3_target_id": f.target, "driver": "volume", "cron_expr": "0 2 * * *", "volume": "data"}),
            "compose volume",
        ),
        (
            json!({"app_id": f.web_app, "s3_target_id": f.target, "driver": "volume", "cron_expr": "0 2 * * *", "volume": "nope"}),
            "unknown volume",
        ),
        (
            json!({"app_id": f.pg_app, "s3_target_id": f.target, "driver": "postgres", "cron_expr": "0 2 * * *", "compose_service": "x"}),
            "unknown service",
        ),
        (
            json!({"app_id": f.pg_app, "s3_target_id": "nope", "driver": "postgres", "cron_expr": "0 2 * * *"}),
            "unknown target",
        ),
    ] {
        let res = create(body).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{why}: {}", res.text());
    }

    // Postgres dump of a compose blueprint, with an explicit password.
    let res = create(
        json!({"app_id": f.pg_app, "s3_target_id": f.target, "driver": "postgres",
                            "cron_expr": "30 2 * * *", "retention_count": 3, "db_password": "pw"}),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.text());
    let job = res.json();
    assert_eq!(job["has_db_password"], true);
    let next = job["next_run_at"]
        .as_str()
        .expect("enabled jobs report their next run");
    assert!(next.contains("T02:30:00"), "{next}");
    assert!(!res.text().contains("\"pw\""));
    let job_id = job["id"].as_str().unwrap().to_string();

    let res = t
        .post(
            &format!("/api/v1/backups/{job_id}/run"),
            json!({}),
            Some(auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::ACCEPTED);
    let run = wait_run(t, auth, &job_id).await;
    assert_eq!(run["status"], "succeeded", "{run}");
    assert_eq!(run["trigger"], "manual");
    assert_eq!(run["size_bytes"], 1234);

    let spec = f.t.fake.backups.lock().unwrap()[0].clone();
    match &spec.source {
        BackupSource::Postgres {
            container,
            db_password,
            db_name,
            ..
        } => {
            assert_eq!(container, "forgenerd-main-db-postgres-1");
            assert_eq!(db_password.as_deref(), Some("pw"));
            assert_eq!(*db_name, None, "defaults come from the container env");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(spec.destination.access_key, "AKIDEXAMPLE");
    assert_eq!(spec.destination.secret_key, "super-secret");
    assert_eq!(spec.destination.retention_count, 3);
    assert_eq!(
        spec.destination.prefix,
        format!("backups/main-db/{job_id}/")
    );
    assert!(spec.object_key.starts_with(&spec.destination.prefix));
    assert!(
        spec.object_key.ends_with("-postgres.dump"),
        "{}",
        spec.object_key
    );
    assert_eq!(run["object_key"], spec.object_key);

    // Volume backup of an image app.
    let res = create(
        json!({"app_id": f.web_app, "s3_target_id": f.target, "driver": "volume",
                            "cron_expr": "0 * * * *", "volume": "uploads"}),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.text());
    let vol_job = res.json()["id"].as_str().unwrap().to_string();
    t.post(
        &format!("/api/v1/backups/{vol_job}/run"),
        json!({}),
        Some(auth),
    )
    .await;
    assert_eq!(wait_run(t, auth, &vol_job).await["status"], "succeeded");
    match &f.t.fake.backups.lock().unwrap()[1].source {
        BackupSource::Volume { volume_name } => assert_eq!(volume_name, "forgenerd-web-uploads"),
        other => panic!("{other:?}"),
    }

    // A target in use cannot be deleted; disabling and editing jobs works.
    assert_eq!(
        t.delete(&format!("/api/v1/s3-targets/{}", f.target), auth)
            .await
            .status,
        StatusCode::CONFLICT
    );
    let res = t
        .patch(
            &format!("/api/v1/backups/{vol_job}"),
            json!({"enabled": false, "cron_expr": "15 4 * * 0"}),
            auth,
        )
        .await
        .json();
    assert_eq!(res["enabled"], false);
    assert_eq!(
        res["next_run_at"],
        Value::Null,
        "paused jobs have no next run"
    );
    assert_eq!(res["cron_expr"], "15 4 * * 0");
}

#[tokio::test]
async fn scheduler_picks_due_jobs_only() {
    use chrono::TimeZone;
    let f = fixture().await;
    let (t, auth) = (&f.t, &f.auth);
    let mk = |cron: &str| json!({"app_id": f.pg_app, "s3_target_id": f.target, "driver": "postgres", "cron_expr": cron});
    let nightly = t
        .post("/api/v1/backups", mk("30 2 * * *"), Some(auth))
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let hourly = t
        .post("/api/v1/backups", mk("0 * * * *"), Some(auth))
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let disabled = t
        .post("/api/v1/backups", mk("30 2 * * *"), Some(auth))
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    t.patch(
        &format!("/api/v1/backups/{disabled}"),
        json!({"enabled": false}),
        auth,
    )
    .await;

    let at = |h, m, s| chrono::Utc.with_ymd_and_hms(2026, 11, 24, h, m, s).unwrap();
    assert_eq!(
        backup::due_jobs(&t.state, at(2, 30, 7)).await.unwrap(),
        vec![nightly.clone()]
    );
    assert_eq!(
        backup::due_jobs(&t.state, at(3, 0, 0)).await.unwrap(),
        vec![hourly]
    );
    assert!(
        backup::due_jobs(&t.state, at(2, 31, 0))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn failed_backups_are_recorded_and_notified() {
    let f = fixture().await;
    let (t, auth) = (&f.t, &f.auth);

    let received = Arc::new(Mutex::new(Vec::<Value>::new()));
    let store = received.clone();
    let sink = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let store = store.clone();
            async move {
                store.lock().unwrap().push(body);
                StatusCode::NO_CONTENT
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, sink).await.unwrap() });
    t.post(
        "/api/v1/notifications",
        json!({"kind": "generic", "url": url}),
        Some(auth),
    )
    .await;

    *f.t.fake.backup_error.lock().unwrap() = Some("S3 returned 403 Forbidden: AccessDenied".into());
    let job = t
        .post("/api/v1/backups", json!({"app_id": f.pg_app, "s3_target_id": f.target, "driver": "postgres", "cron_expr": "0 2 * * *"}), Some(auth))
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    t.post(&format!("/api/v1/backups/{job}/run"), json!({}), Some(auth))
        .await;
    let run = wait_run(t, auth, &job).await;
    assert_eq!(run["status"], "failed");
    assert!(run["error"].as_str().unwrap().contains("AccessDenied"));

    for _ in 0..200 {
        if !received.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let events = received.lock().unwrap().clone();
    assert_eq!(events[0]["event"], "backup.failed");
    assert!(
        events[0]["message"]
            .as_str()
            .unwrap()
            .contains("backup of app `main-db` failed")
    );

    let detail = t
        .get(&format!("/api/v1/backups/{job}"), Some(auth))
        .await
        .json();
    assert_eq!(detail["last_run"]["status"], "failed");
}

#[tokio::test]
async fn restores_replace_data_from_a_succeeded_run() {
    use forgenerd::executor::ContainerAction;

    let f = fixture().await;
    let (t, auth) = (&f.t, &f.auth);
    let deployment = t
        .post(
            &format!("/api/v1/apps/{}/deploy", f.web_app),
            json!({}),
            Some(auth),
        )
        .await
        .json();
    t.wait_deployment(deployment["deployment_id"].as_str().unwrap(), auth)
        .await;
    let job = t
        .post(
            "/api/v1/backups",
            json!({"app_id": f.web_app, "s3_target_id": f.target, "driver": "volume",
                   "cron_expr": "0 * * * *", "volume": "uploads"}),
            Some(auth),
        )
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    t.post(&format!("/api/v1/backups/{job}/run"), json!({}), Some(auth))
        .await;
    let run = wait_run(t, auth, &job).await;
    assert_eq!(run["status"], "succeeded");
    let run_id = run["id"].as_str().unwrap();

    let res = t
        .post(
            &format!("/api/v1/backups/{job}/runs/{run_id}/restore"),
            json!({}),
            Some(auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.text());
    let restores = f.t.fake.restores.lock().unwrap().clone();
    assert_eq!(restores.len(), 1);
    assert_eq!(restores[0].object_key, run["object_key"].as_str().unwrap());
    assert!(matches!(
        &restores[0].target,
        BackupSource::Volume { volume_name } if volume_name == "forgenerd-web-uploads"
    ));
    // The app was stopped for the restore and started again.
    let actions: Vec<ContainerAction> =
        f.t.fake
            .actions
            .lock()
            .unwrap()
            .iter()
            .map(|(_, a)| *a)
            .collect();
    assert!(matches!(
        actions.as_slice(),
        [ContainerAction::Stop, ContainerAction::Start]
    ));

    // Failed runs and runs of other jobs cannot be restored.
    *f.t.fake.backup_error.lock().unwrap() = Some("disk full".into());
    t.post(&format!("/api/v1/backups/{job}/run"), json!({}), Some(auth))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let failed = wait_run(t, auth, &job).await;
    assert_eq!(failed["status"], "failed");
    let res = t
        .post(
            &format!(
                "/api/v1/backups/{job}/runs/{}/restore",
                failed["id"].as_str().unwrap()
            ),
            json!({}),
            Some(auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    let res = t
        .post(
            &format!("/api/v1/backups/{job}/runs/nope/restore"),
            json!({}),
            Some(auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = t
        .post(
            &format!("/api/v1/backups/{job}/runs/{run_id}/restore"),
            json!({}),
            None,
        )
        .await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn panel_backup_is_validated_and_records_failures() {
    let f = fixture().await;
    let (t, auth) = (&f.t, &f.auth);
    let config = t.get("/api/v1/maintenance/panel-backup", Some(auth)).await;
    assert_eq!(config.status, StatusCode::OK);
    assert_eq!(config.json()["s3_target_id"], Value::Null);
    assert_eq!(config.json()["retention_count"], 14);

    for (body, why) in [
        (
            json!({"s3_target_id": null, "cron_expr": "0 3 * * *", "retention_count": 7}),
            "cron without target",
        ),
        (
            json!({"s3_target_id": "nope", "cron_expr": null, "retention_count": 7}),
            "unknown target",
        ),
        (
            json!({"s3_target_id": f.target, "cron_expr": "bad", "retention_count": 7}),
            "cron",
        ),
        (
            json!({"s3_target_id": f.target, "cron_expr": null, "retention_count": 0}),
            "retention",
        ),
    ] {
        let res = t
            .patch("/api/v1/maintenance/panel-backup", body, auth)
            .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{why}: {}", res.text());
    }
    let res = t
        .patch(
            "/api/v1/maintenance/panel-backup",
            json!({"s3_target_id": f.target, "cron_expr": " 0 3 * * * ", "retention_count": 7}),
            auth,
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.text());
    assert_eq!(res.json()["cron_expr"], "0 3 * * *");

    // Nothing listens on the fixture endpoint, so the upload fails and is recorded.
    let res = t
        .post(
            "/api/v1/maintenance/panel-backup/run",
            json!({}),
            Some(auth),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.text());
    assert_eq!(res.json()["ok"], false);
    assert!(
        res.json()["error"]
            .as_str()
            .unwrap()
            .contains("S3 request failed")
    );
    let config = t
        .get("/api/v1/maintenance/panel-backup", Some(auth))
        .await
        .json();
    assert_eq!(config["last_run"]["ok"], false);
    // Temporary snapshots never outlive the run.
    let tmp = f.t.state.data_dir.join("backups/tmp");
    assert_eq!(std::fs::read_dir(tmp).unwrap().count(), 0);
}
