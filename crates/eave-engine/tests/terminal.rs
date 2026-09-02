//! Slice AG1 #16: the pty a provider's TUI runs in.
//!
//! These drive `TerminalHost` against real processes rather than a
//! provider CLI. That is deliberate: what is under test here is the pty —
//! bytes out, keystrokes in, resize, and exit — and a real TUI would make
//! every assertion depend on somebody else's rendering. The provider
//! argv is measured and pinned in `eave-core`'s registry tests; the live
//! handover is a walk, not a unit test.

use std::time::Duration;

use eave_engine::terminal::TerminalHost;

/// Read from an attachment until `needle` shows up or the window closes.
async fn wait_for(host: &TerminalHost, id: &str, needle: &str, window: Duration) -> String {
    let mut att = host.attach(id).expect("attached");
    let mut seen = String::from_utf8_lossy(&att.replay).to_string();
    let deadline = tokio::time::Instant::now() + window;
    while !seen.contains(needle) && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), att.output.recv()).await {
            Ok(Ok(chunk)) => seen.push_str(&String::from_utf8_lossy(&chunk)),
            // Closed/lagged: fall through and let the deadline decide, so
            // the failure message carries what was actually seen.
            Ok(Err(_)) => break,
            Err(_) => continue,
        }
    }
    seen
}

#[tokio::test]
async fn a_pty_carries_what_the_child_wrote() {
    let host = TerminalHost::default();
    host.open(
        "s1",
        std::path::Path::new("/bin/echo"),
        &["hello-from-the-pty".to_string()],
        std::path::Path::new("/tmp"),
        80,
        24,
    )
    .expect("opened");
    let seen = wait_for(&host, "s1", "hello-from-the-pty", Duration::from_secs(5)).await;
    assert!(seen.contains("hello-from-the-pty"), "saw {seen:?}");
    host.close("s1").await;
}

/// The half that makes it a terminal rather than a log: what the operator
/// types reaches the child's stdin.
#[tokio::test]
async fn a_keystroke_reaches_the_child() {
    let host = TerminalHost::default();
    let pty = host
        .open(
            "s2",
            std::path::Path::new("/bin/sh"),
            &[
                "-c".to_string(),
                "read line; echo \"you-typed:$line\"".to_string(),
            ],
            std::path::Path::new("/tmp"),
            80,
            24,
        )
        .expect("opened");
    // A pty is line-disciplined, so the newline is what commits the line —
    // without it `read` waits forever and this test would time out rather
    // than fail with a message.
    tokio::time::sleep(Duration::from_millis(300)).await;
    pty.write(b"abc\r").expect("wrote");
    let seen = wait_for(&host, "s2", "you-typed:abc", Duration::from_secs(5)).await;
    assert!(seen.contains("you-typed:abc"), "saw {seen:?}");
    host.close("s2").await;
}

/// **The property the whole engine-side design exists for**: the pty
/// outlives the window. A client that attaches late gets everything that
/// was drawn before it arrived, which is what makes closing and reopening
/// eave — or crashing it — not lose the conversation.
#[tokio::test]
async fn a_late_attach_replays_what_it_missed() {
    let host = TerminalHost::default();
    host.open(
        "s3",
        std::path::Path::new("/bin/echo"),
        &["drawn-before-anyone-looked".to_string()],
        std::path::Path::new("/tmp"),
        80,
        24,
    )
    .expect("opened");
    // Nobody is attached while this runs.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let att = host.attach("s3").expect("attached");
    let replay = String::from_utf8_lossy(&att.replay).to_string();
    assert!(
        replay.contains("drawn-before-anyone-looked"),
        "a late attach must replay, saw {replay:?}"
    );
    host.close("s3").await;
}

/// The pty is created at the size the client will paint at, not at 80×24
/// and then resized — a TUI that starts small and is corrected redraws
/// visibly, and the first thing an operator sees should not be a jump.
#[tokio::test]
async fn a_pty_is_born_at_the_size_it_was_asked_for() {
    let host = TerminalHost::default();
    let pty = host
        .open(
            "s4",
            std::path::Path::new("/bin/sh"),
            &["-c".to_string(), "stty size; sleep 1".to_string()],
            std::path::Path::new("/tmp"),
            132,
            43,
        )
        .expect("opened");
    assert_eq!(pty.size(), (132, 43));
    // And the CHILD agrees — `stty size` reports what the kernel told it,
    // which is the only proof that the size reached the process rather
    // than just eave's own bookkeeping.
    let seen = wait_for(&host, "s4", "43 132", Duration::from_secs(5)).await;
    assert!(seen.contains("43 132"), "child saw {seen:?}");
    host.close("s4").await;
}

/// A resize must reach the kernel, because that is what raises SIGWINCH
/// and makes the TUI redraw.
#[tokio::test]
async fn a_resize_reaches_the_child() {
    let host = TerminalHost::default();
    let pty = host
        .open(
            "s5",
            std::path::Path::new("/bin/sh"),
            &[
                "-c".to_string(),
                // Report the size again on SIGWINCH.
                "trap 'stty size' WINCH; stty size; sleep 3".to_string(),
            ],
            std::path::Path::new("/tmp"),
            80,
            24,
        )
        .expect("opened");
    tokio::time::sleep(Duration::from_millis(400)).await;
    pty.resize(100, 30).expect("resized");
    assert_eq!(pty.size(), (100, 30));
    let seen = wait_for(&host, "s5", "30 100", Duration::from_secs(5)).await;
    assert!(seen.contains("30 100"), "child saw {seen:?}");
    host.close("s5").await;
}

/// A TUI that exits is a fact the surface has to act on, so the host must
/// stop claiming it is alive.
#[tokio::test]
async fn a_child_that_exits_stops_being_alive() {
    let host = TerminalHost::default();
    let pty = host
        .open(
            "s6",
            std::path::Path::new("/bin/echo"),
            &["bye".to_string()],
            std::path::Path::new("/tmp"),
            80,
            24,
        )
        .expect("opened");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while pty.is_alive() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!pty.is_alive(), "the pty never noticed its child exited");
    assert!(
        !host.open_sessions().contains(&"s6".to_string()),
        "a dead pty must not be reported as an open terminal"
    );
    host.close("s6").await;
}

/// eave's own variables never reach a child (spec 01 §Spawn). A TUI is
/// the most likely place for this to matter, because an operator can
/// start another eave from inside one.
#[tokio::test]
async fn the_child_does_not_inherit_eaves_own_environment() {
    std::env::set_var("EAVE_TERMINAL_TEST_CANARY", "leaked");
    let host = TerminalHost::default();
    host.open(
        "s7",
        std::path::Path::new("/bin/sh"),
        &[
            "-c".to_string(),
            "echo canary=[${EAVE_TERMINAL_TEST_CANARY:-unset}]".to_string(),
        ],
        std::path::Path::new("/tmp"),
        80,
        24,
    )
    .expect("opened");
    let seen = wait_for(&host, "s7", "canary=[", Duration::from_secs(5)).await;
    assert!(
        seen.contains("canary=[unset]"),
        "an EAVE_ variable reached the TUI: {seen:?}"
    );
    host.close("s7").await;
    std::env::remove_var("EAVE_TERMINAL_TEST_CANARY");
}

/// A curses TUI refuses to start without a usable TERM, so eave states
/// one. This is the difference between a terminal that works and one that
/// opens on "terminal not fully functional".
#[tokio::test]
async fn the_child_is_told_what_terminal_it_is_talking_to() {
    let host = TerminalHost::default();
    host.open(
        "s8",
        std::path::Path::new("/bin/sh"),
        &["-c".to_string(), "echo term=[$TERM]".to_string()],
        std::path::Path::new("/tmp"),
        80,
        24,
    )
    .expect("opened");
    let seen = wait_for(&host, "s8", "term=[", Duration::from_secs(5)).await;
    assert!(seen.contains("term=[xterm-256color]"), "saw {seen:?}");
    host.close("s8").await;
}

/// Opening twice is the ordinary consequence of a second window, or of a
/// click on a control the first click already actioned. It must hand back
/// the pty that is already running rather than starting a second CLI on
/// one conversation.
#[tokio::test]
async fn opening_a_second_time_returns_the_running_pty() {
    let host = TerminalHost::default();
    let first = host
        .open(
            "s9",
            std::path::Path::new("/bin/sh"),
            &["-c".to_string(), "sleep 3".to_string()],
            std::path::Path::new("/tmp"),
            80,
            24,
        )
        .expect("opened");
    let second = host
        .open(
            "s9",
            std::path::Path::new("/bin/sh"),
            &["-c".to_string(), "sleep 3".to_string()],
            std::path::Path::new("/tmp"),
            120,
            40,
        )
        .expect("opened again");
    assert!(
        std::sync::Arc::ptr_eq(&first, &second),
        "a second open started a second process on one conversation"
    );
    assert_eq!(second.size(), (80, 24), "the running pty kept its own size");
    host.close("s9").await;
}

// ---------------------------------------------------------------------------
// The engine's handover decisions, against a real engine process.

mod handover {
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    use eave_engine::client::Client;

    fn scratch() -> PathBuf {
        let dir = PathBuf::from("/tmp/eave-wire-tests").join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct EngineProc {
        child: Child,
        data_dir: PathBuf,
    }

    impl EngineProc {
        fn spawn(data_dir: &Path) -> Self {
            Self::spawn_in_mode(data_dir, "normal")
        }

        fn spawn_in_mode(data_dir: &Path, mode: &str) -> Self {
            let engined = PathBuf::from(env!("CARGO_BIN_EXE_eave-engined"));
            let fake = engined.parent().expect("bin dir").join("fake-agent");
            let log = std::fs::File::create(data_dir.join("engine-test.log")).unwrap();
            let child = Command::new(&engined)
                .args(["--data-dir", &data_dir.display().to_string()])
                // Never the developer's checkout (`d6f98c1`).
                .current_dir(data_dir)
                .env("EAVE_AGENT", &fake)
                .env("EAVE_FAKE_MODE", mode)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(log)
                .spawn()
                .expect("spawn eave-engined");
            let me = Self {
                child,
                data_dir: data_dir.to_path_buf(),
            };
            let sock = eave_wire::socket_path(&me.data_dir);
            let deadline = Instant::now() + Duration::from_secs(20);
            while std::os::unix::net::UnixStream::connect(&sock).is_err() {
                assert!(Instant::now() < deadline, "engine never opened its socket");
                std::thread::sleep(Duration::from_millis(20));
            }
            me
        }
    }

    impl Drop for EngineProc {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// **A session that has never run cannot be handed over, and the
    /// refusal is the feature.**
    ///
    /// The TUI would start perfectly well — and would mint a conversation
    /// under an id the provider chooses and eave never learns, because
    /// eave only ever learns one from the `Init` frame on its own wire.
    /// The operator would come back to an empty transcript beside a real
    /// conversation with no way to reconnect them. Refusing states that
    /// and says what to do instead.
    #[tokio::test]
    async fn a_session_that_never_ran_is_refused_with_what_to_do_instead() {
        let dir = scratch();
        let _e = EngineProc::spawn(&dir);
        let c = Client::connect(&dir).await.expect("connect");
        let info = c
            .open(None, Some(dir.display().to_string()))
            .await
            .expect("open");
        let id = info.session_id;

        let reply = c.term_open(&id, 80, 24).await.expect("asked");
        assert!(!reply.opened, "a never-run session was handed over");
        assert!(
            !reply.unsupported,
            "this is a 'not yet', not a 'cannot' — the provider has a TUI"
        );
        let reason = reply.reason.expect("a refusal states its reason");
        assert!(
            reason.contains("has not run yet") && reason.contains("send one message"),
            "the sentence must say what to do instead, got {reason:?}"
        );
    }

    /// The engine refuses a provider whose TUI eave cannot point at a
    /// conversation, and forwards that provider's OWN measured sentence
    /// rather than a house paraphrase.
    #[tokio::test]
    async fn a_provider_with_no_usable_tui_is_refused_in_its_own_words() {
        let dir = scratch();
        let _e = EngineProc::spawn(&dir);
        let c = Client::connect(&dir).await.expect("connect");
        let agent = eave_core::session::AgentConfig {
            provider: Some("cursor".into()),
            ..Default::default()
        };
        let Ok(info) = c
            .open_with_agent(None, Some(dir.display().to_string()), Some(agent))
            .await
        else {
            // Cursor's posture gate may refuse the open on this machine;
            // that is a different slice's business and not a failure here.
            return;
        };
        let id = info.session_id;
        let reply = c.term_open(&id, 80, 24).await.expect("asked");
        assert!(!reply.opened);
        assert!(
            reply.unsupported,
            "an absent TUI is a permanent answer, not a refusal that would \
             succeed later"
        );
        let reason = reply.reason.expect("a refusal states its reason");
        assert!(
            reason.contains("chatId"),
            "the operator should read the sentence measured against THEIR \
             CLI, not a house paraphrase — got {reason:?}"
        );
    }

    /// **The whole path, over the wire.**
    ///
    /// Everything else in this file tests one joint: the pty host in
    /// isolation, or the engine's refusals without a process. This drives
    /// what an operator's click actually does — settle the agent, spawn a
    /// pty, attach, and get bytes back — through the real socket.
    ///
    /// The "TUI" here is eave's own fake agent, which is not one. That is
    /// deliberate and it is what makes the test hermetic: what is under
    /// test is eave's plumbing, and a real provider CLI would make every
    /// assertion depend on somebody else's rendering and somebody else's
    /// login. The argv each provider is handed is measured and pinned in
    /// `eave-core`'s registry tests; landing in a live TUI is a walk.
    #[tokio::test]
    async fn a_handover_settles_the_agent_spawns_a_pty_and_streams_it() {
        use eave_wire::SubscriptionItem;

        let dir = scratch();
        let _e = EngineProc::spawn(&dir);
        let c = Client::connect(&dir).await.expect("connect");
        let info = c
            .open(None, Some(dir.display().to_string()))
            .await
            .expect("open");
        let id = info.session_id;

        // A session has to have RUN for eave to know the provider's own id
        // for the conversation — that is the refusal two tests above.
        c.send(&id, "hello").await.expect("send");
        let (_sub, mut rx) = c.subscribe_with_id(&id, None).await.expect("subscribe");
        let deadline = Instant::now() + Duration::from_secs(20);
        let (mut saw_init, mut settled) = (false, false);
        while !(saw_init && settled) {
            assert!(
                Instant::now() < deadline,
                "the agent never announced a session and settled (init={saw_init} settled={settled})"
            );
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Ok(Some(item)) = tokio::time::timeout(remaining, rx.recv()).await else {
                break;
            };
            if let SubscriptionItem::Events { records, .. } = item {
                for r in &records {
                    match &r.event {
                        eave_core::journal::JournalEvent::Provider {
                            event: eave_core::events::SessionEvent::Init { .. },
                        } => saw_init = true,
                        // A turn in flight is refused, so the handover
                        // cannot be reached until this arrives.
                        eave_core::journal::JournalEvent::Provider {
                            event: eave_core::events::SessionEvent::TurnCompleted { .. },
                        } => settled = true,
                        eave_core::journal::JournalEvent::Closed { .. } => settled = true,
                        _ => {}
                    }
                }
            }
        }
        assert!(
            saw_init,
            "no Init record, so there is no cursor to hand over"
        );

        let reply = c.term_open(&id, 100, 30).await.expect("asked");
        assert!(reply.opened, "the handover was refused: {:?}", reply.reason);
        assert_eq!(
            (reply.cols, reply.rows),
            (100, 30),
            "the pty is born at the size the panel will paint at"
        );

        // The rail must now say so — this is the honesty claim, measured
        // through the same `list` a client reads.
        let sessions = c.list().await.expect("list");
        let me = sessions
            .iter()
            .find(|s| s.session_id == id)
            .expect("session");
        assert_eq!(
            me.state, "terminal",
            "a handed-over session must not report a state eave cannot see"
        );

        // And attaching streams it.
        let (_tid, mut trx) = c.term_attach(&id).await.expect("attach");
        let first = tokio::time::timeout(Duration::from_secs(10), trx.recv())
            .await
            .expect("a screen in time")
            .expect("the terminal stream ended");
        match first {
            SubscriptionItem::TermScreen { cols, rows, .. } => {
                assert_eq!((cols, rows), (100, 30));
            }
            other => panic!("the first frame must be the replay, got {other:?}"),
        }

        // Closing gives the session back, and the rail stops saying
        // `terminal` — a claim that outlived the pty would be the same lie
        // from the other side.
        let closed = c.term_close(&id).await.expect("close");
        assert!(closed.ok, "there was a pty to close");
        let sessions = c.list().await.expect("list");
        let me = sessions
            .iter()
            .find(|s| s.session_id == id)
            .expect("session");
        assert_ne!(me.state, "terminal", "the handover claim outlived the pty");
    }

    /// **A turn in flight is REFUSED, not interrupted.**
    ///
    /// The same call AG1 #10 made about rewinding through a running turn,
    /// for the same reason: killing the CLI mid-turn leaves a turn no
    /// `TurnCompleted` can ever close, and the attention ledger's
    /// documented hole is exactly there. Manufacturing the one input it
    /// cannot survive, to save a click, is a bad trade — the operator
    /// presses Stop first, through the interrupt path that settles a turn
    /// properly.
    ///
    /// `flood` is the fake agent that emits agent work and never sends a
    /// result, which is exactly an open turn. (`linger` completes each
    /// turn and then idles, so a session running under it has no turn in
    /// flight to protect — an earlier draft of this test used it and
    /// proved nothing.)
    #[tokio::test]
    async fn a_turn_in_flight_is_refused_rather_than_killed() {
        let dir = scratch();
        let _e = EngineProc::spawn_in_mode(&dir, "flood");
        let c = Client::connect(&dir).await.expect("connect");
        let info = c
            .open(None, Some(dir.display().to_string()))
            .await
            .expect("open");
        let id = info.session_id;
        c.send(&id, "hello").await.expect("send");

        // Poll until the guard fires. The alternative — sleeping a fixed
        // moment and asserting once — passes on a slow machine for the
        // wrong reason, because the refusal it would read is the
        // never-run one.
        let deadline = Instant::now() + Duration::from_secs(20);
        let reason = loop {
            let reply = c.term_open(&id, 80, 24).await.expect("asked");
            assert!(
                !reply.opened,
                "a running turn was killed to open a terminal"
            );
            let reason = reply.reason.expect("a refusal states its reason");
            if reason.contains("press Stop first") {
                break reason;
            }
            assert!(
                Instant::now() < deadline,
                "the turn guard never fired; the last refusal was {reason:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert!(reason.contains("press Stop first"));

        // And the session is untouched — still eave's, never handed over.
        let sessions = c.list().await.expect("list");
        let me = sessions
            .iter()
            .find(|s| s.session_id == id)
            .expect("session");
        assert_ne!(
            me.state, "terminal",
            "a refused handover must leave no trace of itself"
        );
    }

    /// **The rail must not claim to know.** eave decodes no frames while a
    /// session is handed over, so `running` and `quiet` are both claims
    /// about activity it cannot see.
    ///
    /// Driven through the pty host directly rather than through a real
    /// provider CLI: what is under test is the engine's reported state,
    /// and a live TUI would make it depend on somebody else's binary.
    #[tokio::test]
    async fn an_unopened_session_is_never_reported_as_a_terminal() {
        let dir = scratch();
        let _e = EngineProc::spawn(&dir);
        let c = Client::connect(&dir).await.expect("connect");
        let info = c
            .open(None, Some(dir.display().to_string()))
            .await
            .expect("open");
        let id = info.session_id;
        let sessions = c.list().await.expect("list");
        let me = sessions
            .iter()
            .find(|s| s.session_id == id)
            .expect("the session we just opened");
        assert_ne!(
            me.state, "terminal",
            "a session nobody handed over must not claim to be one"
        );
    }
}

/// **The order of `term_open`'s two refusals, pinned in the source.**
///
/// A session whose FIRST turn is still running has no `Init` on its
/// journal yet, so a cursor check placed first answers "this session has
/// not run yet" about a session the operator is watching run. Both
/// sentences are true of the journal; only one is true of what is on
/// screen.
///
/// Pinned structurally because the live path cannot reach it: every fake
/// agent emits its `init` in the same millisecond it starts, so the window
/// where the two guards disagree does not exist in the fixture. It is a
/// real window against a real CLI — the whole point of the guard is that a
/// turn can be in flight before anything has been journalled about it —
/// and a source test is the honest way to hold an order the harness cannot
/// otherwise produce. The same idiom `files_diffstat`'s deletion is held
/// by.
#[test]
fn the_turn_guard_is_checked_before_the_cursor_guard() {
    let src = include_str!("../src/engine.rs");
    let body = src
        .split("async fn term_open(")
        .nth(1)
        .expect("term_open is still in engine.rs");
    let turn = body
        .find("press Stop first")
        .expect("the turn-in-flight refusal");
    let cursor = body.find("has not run yet").expect("the never-run refusal");
    assert!(
        turn < cursor,
        "the never-run refusal now precedes the turn-in-flight one, so a \
         session whose first turn is still running is told it has not run"
    );
}
