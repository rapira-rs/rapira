use php_sys::{Mode, Rapira};
use tests::{drain_async, fixture, php_lock_async, req};

#[tokio::test]
async fn worker_survives_exit() -> anyhow::Result<()> {
    let _guard = php_lock_async().await;

    let r = Rapira::start(Mode::Worker(fixture("shared/bailout-worker.php")))?;
    let h = r.handle();
    let (s1, b1) = drain_async(
        h.handle(req("/?boom=0", "shared/bailout-worker.php"))
            .await?,
    )
    .await;
    let (s2, b2) = drain_async(
        h.handle(req("/?boom=1", "shared/bailout-worker.php"))
            .await?,
    )
    .await;
    let (s3, b3) = drain_async(
        h.handle(req("/?boom=0", "shared/bailout-worker.php"))
            .await?,
    )
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

    let r = Rapira::start(Mode::Worker(fixture("shared/worker.php")))?;

    let producers: Vec<_> = (0..24)
        .map(|t| {
            let h: php_sys::RapiraHandle = r.handle();
            tokio::spawn(async move {
                for i in 0..256 {
                    let name: String = format!("t{t}-r{i}");
                    let rx = h
                        .handle(req(&format!("/?name={name}"), "shared/worker.php"))
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
