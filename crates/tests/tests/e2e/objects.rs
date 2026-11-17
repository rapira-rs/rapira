use rapira_sapi::plugin::Mode;
use tests::wire::submit;
use tests::{StatusBody, drain, fixture, req};

use crate::harness::Spawn;

/// `count_objects()` and `get_objects()` read the object store of the worker. Each case is one request: the fixture builds the objects and answers one line.
#[test]
fn object_functions_report_the_live_objects() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        query: &'static str,
        body: &'static str,
    }
    let cases = [
        Case {
            name: "count_objects() keys by the exact class, so a subclass has its own key",
            query: "/?case=count-per-class",
            body: "Leak=2 SubLeak=1",
        },
        Case {
            name: "count_objects() has no key for a class without a live object",
            query: "/?case=count-no-instance",
            body: "absent",
        },
        Case {
            name: "count_objects() counts a garbage cycle until the collector runs",
            query: "/?case=count-cycle",
            body: "garbage=2 collected=0",
        },
        Case {
            name: "count_objects() keys an anonymous class by its ::class name",
            query: "/?case=count-anonymous",
            body: "1",
        },
        Case {
            name: "get_objects() without a class list returns the counted objects, without the map itself",
            query: "/?case=get-all",
            body: "counted=same self=no",
        },
        Case {
            name: "get_objects() matches by instanceof: a parent class and an interface",
            query: "/?case=get-instanceof",
            body: "class=2 interface=1 sub=yes",
        },
        Case {
            name: "get_objects() matches an object of any listed class",
            query: "/?case=get-any-of",
            body: "matched=1",
        },
        Case {
            name: "get_objects() maps each object to null",
            query: "/?case=get-values",
            body: "null,null",
        },
        Case {
            name: "get_objects() matches nothing for a class that is not loaded, and loads nothing",
            query: "/?case=get-unloaded-name",
            body: "matched=0 autoloaded=0",
        },
        Case {
            name: "get_objects() holds the objects weakly",
            query: "/?case=get-weak",
            body: "held=1 released=0",
        },
        Case {
            name: "get_objects() returns a lazy object uninitialized",
            query: "/?case=get-lazy",
            body: "matched=1 uninitialized",
        },
        Case {
            name: "both functions skip the iterator object of the engine during a foreach",
            query: "/?case=foreach-over-object",
            body: "registered",
        },
        Case {
            name: "get_objects() rejects an object in the class list",
            query: "/?case=get-object-entry",
            body: "Rapira\\get_objects(): Argument #1 ($classes) must be an array of class names, Leak given",
        },
        Case {
            name: "get_objects() rejects an integer in the class list",
            query: "/?case=get-int-entry",
            body: "Rapira\\get_objects(): Argument #1 ($classes) must be an array of class names, int given",
        },
    ];
    let srv = Spawn::http(Mode::Worker, fixture("objects/worker.php")).spawn();

    for case in cases {
        let StatusBody { status, body } = drain(submit(srv.addr, req(case.query))?);
        assert_eq!((status, body.as_str()), (200, case.body), "{}", case.name);
    }
    Ok(())
}
