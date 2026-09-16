use php_sys::{Mode, Rapira};
use tests::{drain, fixture, php_lock, req};

mod native_trace {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use tracing::{Id, Subscriber};
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::registry::LookupSpan;

    #[derive(Debug)]
    pub struct Span {
        pub name: &'static str,
        pub parent: Option<Id>,
        pub opened: usize,
        pub closed: Option<usize>,
        pub error: bool,
    }

    #[derive(Default)]
    pub struct Records {
        pub sequence: usize,
        pub spans: HashMap<Id, Span>,
        pub events: Vec<(String, Option<Id>)>,
    }

    pub fn records() -> std::sync::MutexGuard<'static, Records> {
        static RECORDS: OnceLock<Mutex<Records>> = OnceLock::new();
        RECORDS
            .get_or_init(|| Mutex::new(Records::default()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[derive(Default)]
    struct Fields {
        message: String,
        error: bool,
    }

    impl tracing::field::Visit for Fields {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            match field.name() {
                "otel.status_code" => self.error = value == "ERROR",
                "message" => self.message = value.to_owned(),
                _ => {}
            }
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.message = format!("{value:?}");
            }
        }
    }

    struct Capture;

    impl<S> tracing_subscriber::Layer<S> for Capture
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_new_span(&self, attrs: &tracing::span::Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
            let mut records = records();
            records.sequence += 1;
            let opened = records.sequence;
            let parent = ctx.span(id).unwrap().parent().map(|span| span.id());
            records.spans.insert(
                id.clone(),
                Span {
                    name: attrs.metadata().name(),
                    parent,
                    opened,
                    closed: None,
                    error: false,
                },
            );
        }

        fn on_record(&self, id: &Id, values: &tracing::span::Record<'_>, _: Context<'_, S>) {
            let mut fields = Fields::default();
            values.record(&mut fields);
            records().spans.get_mut(id).unwrap().error |= fields.error;
        }

        fn on_close(&self, id: Id, _: Context<'_, S>) {
            let mut records = records();
            records.sequence += 1;
            let closed = records.sequence;
            records.spans.get_mut(&id).unwrap().closed = Some(closed);
        }

        fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
            let mut fields = Fields::default();
            event.record(&mut fields);
            records()
                .events
                .push((fields.message, ctx.event_span(event).map(|span| span.id())));
        }
    }

    pub fn init() {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            tracing::subscriber::set_global_default(tracing_subscriber::registry().with(Capture))
                .unwrap();
        });
    }

    pub fn wait_closed(id: &Id, name: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while records().spans.get(id).unwrap().closed.is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "{name}: request span stayed open after completion"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

#[test]
fn native_execution_spans_end_at_request_boundaries() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        dispatcher: bool,
        uri: &'static str,
        error: bool,
    }
    let cases = [
        Case {
            name: "worker completion",
            dispatcher: false,
            uri: "/",
            error: false,
        },
        Case {
            name: "worker early response",
            dispatcher: false,
            uri: "/?finish=1",
            error: false,
        },
        Case {
            name: "worker exit",
            dispatcher: false,
            uri: "/?exit=1",
            error: false,
        },
        Case {
            name: "worker throw",
            dispatcher: false,
            uri: "/?throw=1",
            error: true,
        },
        Case {
            name: "worker fatal",
            dispatcher: false,
            uri: "/?fatal=1",
            error: true,
        },
        Case {
            name: "worker shutdown fatal",
            dispatcher: false,
            uri: "/?shutdown-fatal=1",
            error: true,
        },
        Case {
            name: "retained dispatcher exchange",
            dispatcher: true,
            uri: "/",
            error: false,
        },
        Case {
            name: "dispatcher throw",
            dispatcher: true,
            uri: "/throw",
            error: true,
        },
        Case {
            name: "dispatcher fatal",
            dispatcher: true,
            uri: "/fatal",
            error: true,
        },
        Case {
            name: "dispatcher exit with active exchange",
            dispatcher: true,
            uri: "/exit",
            error: true,
        },
    ];
    let _guard = php_lock();
    native_trace::init();
    for case in cases {
        *native_trace::records() = Default::default();
        let script = if case.dispatcher {
            "dispatcher/native-trace-worker.php"
        } else {
            "worker/native-trace-worker.php"
        };
        let mode = if case.dispatcher {
            Mode::Dispatcher(fixture(script))
        } else {
            Mode::Worker(fixture(script))
        };
        let r = Rapira::start(mode)?;
        let h = r.handle();
        let mut request = req(case.uri, script);
        request.span = tracing::info_span!(parent: None, "test.request");
        let root = request.span.id().unwrap();
        tests::drain_resp(h.handle_blocking(request)?);
        native_trace::wait_closed(&root, case.name);
        let mut next = req("/", script);
        next.span = tracing::info_span!(parent: None, "test.next");
        let next_root = next.span.id().unwrap();
        let response = tests::drain_resp(h.handle_blocking(next)?);
        assert_eq!(response.body_string(), "ok", "{}: next request", case.name);
        native_trace::wait_closed(&next_root, case.name);
        drop(h);
        r.shutdown();

        let records = native_trace::records();
        let children: Vec<_> = records
            .spans
            .iter()
            .filter(|(_, span)| span.parent.as_ref() == Some(&root))
            .collect();
        let (execute_id, execute) = children
            .iter()
            .find(|(_, span)| span.name == "php.execute")
            .expect("PHP execution span");
        let (_, queue) = children
            .iter()
            .find(|(_, span)| span.name == "queue.wait")
            .expect("queue span");
        assert!(
            queue.closed.unwrap() < execute.opened,
            "{}: queue wait must end before PHP execution",
            case.name
        );
        assert_eq!(
            execute.error, case.error,
            "{}: native error status",
            case.name
        );
        assert!(execute.closed.is_some(), "{}", case.name);
        assert!(
            records
                .events
                .iter()
                .any(|(message, span)| message == "trace-active"
                    && span.as_ref() == Some(*execute_id)),
            "{}: PHP log context",
            case.name
        );
        assert!(
            records
                .events
                .iter()
                .filter(|(message, _)| message == "trace-idle")
                .all(|(_, span)| span.is_none()),
            "{}: stale span outside a request",
            case.name
        );
        if !case.dispatcher {
            assert!(
                records
                    .events
                    .iter()
                    .any(|(message, span)| message == "trace-shutdown"
                        && span.as_ref() == Some(*execute_id)),
                "{}: shutdown context",
                case.name
            );
        }
        if case.uri.contains("finish") {
            assert!(
                records
                    .events
                    .iter()
                    .any(|(message, span)| message == "trace-after-response"
                        && span.as_ref() == Some(*execute_id)),
                "{}: early response retains worker context",
                case.name
            );
        }
    }
    Ok(())
}

/// Pins that Http value objects construct and refuse readonly reassignment, wrong arity, and a bad address union.
#[test]
fn value_objects_construct_and_refuse() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("http_values/worker.php")))?;
    let h = r.handle();
    let (status, body) = drain(h.handle_blocking(req("/", "http_values/construct.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200, "construction must succeed (body: {body:?})");
    for line in [
        "POST /upload?x=1 HTTP/2",
        "203.0.113.7:44123",
        "server-path=NULL",
        "note=hello",
        "me.png 512",
        "h2 NULL",
        "example.test:8443 1722700000.25",
        "tls-null: NULL",
        "traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        "tracestate: vendor=a",
        "trace-readonly: enforced",
        "trace-type: enforced",
        "readonly: enforced",
        "arity: enforced",
        "union: enforced",
        "done",
    ] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }
    Ok(())
}
