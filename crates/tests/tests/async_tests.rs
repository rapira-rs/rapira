use rapira_sapi::{Mode, Rapira};
use tests::{drain_async, fixture, php_lock_async, req};

#[tokio::test]
async fn worker_survives_exit() -> anyhow::Result<()> {
    let _guard = php_lock_async().await;

    let r = Rapira::start(Mode::Worker(fixture("shared/bailout-worker.php")), None)?;
    let h = r.sink();
    let (s1, b1) =
        drain_async(tests::submit_async(&h, req("/?boom=0", "shared/bailout-worker.php")).await?)
            .await;
    let (s2, b2) =
        drain_async(tests::submit_async(&h, req("/?boom=1", "shared/bailout-worker.php")).await?)
            .await;
    let (s3, b3) =
        drain_async(tests::submit_async(&h, req("/?boom=0", "shared/bailout-worker.php")).await?)
            .await;

    assert_eq!(s1, 200);
    assert!(b1.contains("ok counter=1"), "req1 (got: {b1:?})");

    assert_eq!(
        s2, 200,
        "exit() is a graceful unwind, not a 500 (got status {s2}, body {b2:?})"
    );
    assert!(
        b2.is_empty(),
        "exit(1) before any output => empty body (got: {b2:?})"
    );

    assert_eq!(s3, 200, "worker must recover after exit() (got {s3})");
    assert!(
        b3.contains("ok counter=3"),
        "worker must survive exit() and serve the next request (got: {b3:?})"
    );
    drop(h);
    drop(r);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn many_producers_test() -> anyhow::Result<()> {
    let _guard = php_lock_async().await;

    let r = Rapira::start(Mode::Worker(fixture("shared/worker.php")), None)?;

    let producers: Vec<_> = (0..24)
        .map(|t| {
            let h: rapira_sapi::work::Sink = r.sink();
            tokio::spawn(async move {
                for i in 0..256 {
                    let name: String = format!("t{t}-r{i}");
                    let rx = tests::submit_async(
                        &h,
                        req(&format!("/?name={name}"), "shared/worker.php"),
                    )
                    .await
                    .expect("ruuuun!");
                    let (status, body) = drain_async(rx).await;
                    assert_eq!(
                        status, 200,
                        "worker must serve (got {status}, body {body:?})"
                    );
                    assert!(
                        body.contains(&format!("Hello from worker, {name}!")),
                        "worker must serve (got: {body:?})"
                    );
                }
            })
        })
        .collect::<Vec<_>>();

    for p in producers {
        if let Err(e) = p.await {
            std::panic::resume_unwind(e.into_panic());
        }
    }

    drop(r);
    Ok(())
}
