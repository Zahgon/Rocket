use super::task::Task;

use rand::{Rng, thread_rng, distributions::Alphanumeric};

use reqwest::header::CONTENT_TYPE;
use reqwest::{Response, StatusCode};

// We use a lock to synchronize between tests so DB operations don't collide.
// For now. In the future, we'll have a nice way to run each test in a DB
// transaction so we can regain concurrency.
static DB_LOCK: parking_lot::Mutex<()> = parking_lot::const_mutex(());

const FORM: &str = "application/x-www-form-urlencoded";

// The framework no longer provides an in-process test client, so the tests
// drive a real server bound to an ephemeral port. Redirects are not followed,
// matching the behavior the assertions below were written against.
struct Client {
    base: String,
    http: reqwest::Client,
}

impl Client {
    async fn tracked(conn: super::DbConn) -> Client {
        let app = super::build(conn).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await
            .expect("failed to bind test server");

        let addr = listener.local_addr().expect("test server address");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("test server");
        });

        let http = reqwest::Client::builder()
            .cookie_store(true)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("test client");

        Client { base: format!("http://{}", addr), http }
    }

    fn url(&self, path: impl AsRef<str>) -> String {
        format!("{}{}", self.base, path.as_ref())
    }

    async fn get(&self, path: impl AsRef<str>) -> Response {
        self.http.get(self.url(path)).send().await.expect("GET failed")
    }

    async fn post(&self, path: impl AsRef<str>, body: impl Into<String>) -> Response {
        self.http.post(self.url(path))
            .header(CONTENT_TYPE, FORM)
            .body(body.into())
            .send()
            .await
            .expect("POST failed")
    }

    async fn put(&self, path: impl AsRef<str>) -> Response {
        self.http.put(self.url(path)).send().await.expect("PUT failed")
    }

    async fn delete(&self, path: impl AsRef<str>) -> Response {
        self.http.delete(self.url(path)).send().await.expect("DELETE failed")
    }
}

fn has_error_cookie(res: &Response) -> bool {
    res.cookies().any(|c| c.value().contains("error"))
}

macro_rules! run_test {
    (|$client:ident, $conn:ident| $block:expr) => ({
        let _lock = DB_LOCK.lock();

        tokio::runtime::Runtime::new().expect("test runtime").block_on(async move {
            let $conn = super::DbConn::new(super::DATABASE_URL);
            let $client = Client::tracked($conn.clone()).await;
            Task::delete_all(&$conn).await.expect("failed to delete all tasks for testing");

            $block
        })
    })
}

#[test]
fn test_index() {
    let _lock = DB_LOCK.lock();

    tokio::runtime::Runtime::new().expect("test runtime").block_on(async move {
        let client = Client::tracked(super::DbConn::new(super::DATABASE_URL)).await;
        let response = client.get("/").await;
        assert_eq!(response.status(), StatusCode::OK);
    })
}

#[test]
fn test_insertion_deletion() {
    run_test!(|client, conn| {
        // Get the tasks before making changes.
        let init_tasks = Task::all(&conn).await.unwrap();

        // Issue a request to insert a new task.
        client.post("/todo", "description=My+first+task").await;

        // Ensure we have one more task in the database.
        let new_tasks = Task::all(&conn).await.unwrap();
        assert_eq!(new_tasks.len(), init_tasks.len() + 1);

        // Ensure the task is what we expect.
        assert_eq!(new_tasks[0].description, "My first task");
        assert_eq!(new_tasks[0].completed, false);

        // Issue a request to delete the task.
        let id = new_tasks[0].id.unwrap();
        client.delete(format!("/todo/{}", id)).await;

        // Ensure it's gone.
        let final_tasks = Task::all(&conn).await.unwrap();
        assert_eq!(final_tasks.len(), init_tasks.len());
        if final_tasks.len() > 0 {
            assert_ne!(final_tasks[0].description, "My first task");
        }
    })
}

#[test]
fn test_toggle() {
    run_test!(|client, conn| {
        // Issue a request to insert a new task; ensure it's not yet completed.
        client.post("/todo", "description=test_for_completion").await;

        let task = Task::all(&conn).await.unwrap()[0].clone();
        assert_eq!(task.completed, false);

        // Issue a request to toggle the task; ensure it is completed.
        client.put(format!("/todo/{}", task.id.unwrap())).await;
        assert_eq!(Task::all(&conn).await.unwrap()[0].completed, true);

        // Issue a request to toggle the task; ensure it's not completed again.
        client.put(format!("/todo/{}", task.id.unwrap())).await;
        assert_eq!(Task::all(&conn).await.unwrap()[0].completed, false);
    })
}

#[test]
fn test_many_insertions() {
    const ITER: usize = 100;

    run_test!(|client, conn| {
        // Get the number of tasks initially.
        let init_num = Task::all(&conn).await.unwrap().len();
        let mut descs = Vec::new();

        for i in 0..ITER {
            // Issue a request to insert a new task with a random description.
            let desc: String = thread_rng()
                .sample_iter(&Alphanumeric)
                .take(12)
                .map(char::from)
                .collect();

            client.post("/todo", format!("description={}", desc)).await;

            // Record the description we choose for this iteration.
            descs.insert(0, desc);

            // Ensure the task was inserted properly and all other tasks remain.
            let tasks = Task::all(&conn).await.unwrap();
            assert_eq!(tasks.len(), init_num + i + 1);

            for j in 0..i {
                assert_eq!(descs[j], tasks[j].description);
            }
        }
    })
}

#[test]
fn test_bad_form_submissions() {
    run_test!(|client, _conn| {
        // Submit an empty form. We should get a 422 but no flash error.
        let res = client.post("/todo", "").await;

        assert!(!has_error_cookie(&res));
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // Submit a form with an empty description. We look for 'error' in the
        // cookies which corresponds to flash message being set as an error.
        let res = client.post("/todo", "description=").await;

        // Check that the flash cookie set and that we're redirected to index.
        assert!(has_error_cookie(&res));
        assert_eq!(res.status(), StatusCode::SEE_OTHER);

        // The flash cookie should still be present and the error message should
        // be rendered the index.
        let body = client.get("/").await.text().await.unwrap();
        assert!(body.contains("Description cannot be empty."));

        // Check that the flash is cleared upon another visit to the index.
        let body = client.get("/").await.text().await.unwrap();
        assert!(!body.contains("Description cannot be empty."));

        // Submit a form without a description. Expect a 422 but no flash error.
        let res = client.post("/todo", "evil=smile").await;

        assert!(!has_error_cookie(&res));
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    })
}
