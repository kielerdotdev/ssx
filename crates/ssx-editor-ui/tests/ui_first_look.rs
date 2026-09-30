mod common;

use common::*;

#[test]
fn first_look() {
    let app = app_for(dashboard());
    let mut h = window(app, [1280.0, 800.0]);
    h.run();
    dump(&mut h, "first-look");
    assert!(h.state().finished.is_none());
}
