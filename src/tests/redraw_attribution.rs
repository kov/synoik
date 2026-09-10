// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Every redraw is credited to whoever asked for it: a client's commit to that client's window,
//! the compositor's own requests to their call site. See [`crate::frame_log::Requester`].

use super::*;
use crate::frame_log::Requester;

#[test]
fn a_commit_charges_its_redraw_to_the_client_window() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().frame_log.enable_for_test();

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);
    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(100, 100);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    let mapped = f
        .synoik()
        .layout
        .windows()
        .next()
        .map(|(_, mapped)| mapped.id().get())
        .expect("the window mapped");

    // A commit on the mapped window, damaging it.
    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(id);

    let output = f.synoik_output(1).name();
    let by = f.synoik().frame_log.redraws_by(&output);
    assert!(
        by.iter().any(|(r, n)| *n > 0
            && matches!(r, Requester::Client { window: Some(w), .. } if *w == mapped)),
        "the commit's redraw is charged to the client's window: {by:?}"
    );
}

#[test]
fn the_compositors_own_request_is_charged_to_its_call_site() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().frame_log.enable_for_test();

    f.synoik().queue_redraw_all();
    f.turn();

    let output = f.synoik_output(1).name();
    let by = f.synoik().frame_log.redraws_by(&output);
    assert!(
        by.iter().any(|(r, n)| *n > 0
            && matches!(r, Requester::Internal(at) if at.file().ends_with("redraw_attribution.rs"))),
        "`#[track_caller]` carries the request back to this file: {by:?}"
    );
}
