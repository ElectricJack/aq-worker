use aq_session_protocol::{Digest, OutputFrame, SessionIdentity, TerminalIdentity};
use aq_terminal_renderer::*;
use uuid::Uuid;
fn terminal() -> TerminalIdentity {
    TerminalIdentity {
        session: SessionIdentity::Central {
            authority_id: Uuid::from_u128(1),
            session_id: Uuid::from_u128(2),
            generation: 1,
        },
        terminal_id: Uuid::from_u128(3),
        epoch: 1,
    }
}
fn renderer(columns: u16, rows: u16) -> Renderer {
    Renderer::from_assumed_blank(AssumedBlank {
        terminal: terminal(),
        columns,
        rows,
    })
    .unwrap()
}
fn frame(sequence: u64, bytes: &[u8]) -> OutputFrame {
    OutputFrame {
        terminal: terminal(),
        sequence,
        byte_length: bytes.len() as u32,
        bytes: bytes.to_vec(),
        digest: Digest::of(bytes),
    }
}
fn line(r: &Renderer, row: usize) -> String {
    r.screen()
        .line(row)
        .unwrap()
        .iter()
        .filter(|c| c.width() != 0)
        .map(|c| if c.glyph().is_empty() { " " } else { c.glyph() })
        .collect::<String>()
        .trim_end()
        .to_string()
}
fn feed(r: &mut Renderer, bytes: &[u8]) {
    r.bytes(
        r.through_ordinal() + 1,
        &frame(r.through_sequence() + 1, bytes),
    )
    .unwrap();
}
#[test]
fn recorded_synthetic_cli_repertoire_is_supported_without_blanket_fallback() {
    let bytes = include_bytes!("fixtures/synthetic-cli.ansi");
    let mut r = renderer(48, 12);
    feed(&mut r, bytes);
    assert!(r.is_consistent());
    assert!(!r.screen().alternate);
    assert_eq!(line(&r, 0), "worker: queued");
    assert_eq!(line(&r, 1), "worker: starting");
    assert_eq!(line(&r, 2), "worker: completed");
    assert_eq!(r.title(), "AQ synthetic interactive worker");
    assert!(r.modes().cursor_visible);
    assert!(!r.modes().application_cursor);
    assert!(!r.modes().bracketed_paste);
    let split = bytes
        .windows(b"\x1b[5;2H".len())
        .position(|p| p == b"\x1b[5;2H")
        .unwrap();
    let mut active = renderer(48, 12);
    feed(&mut active, &bytes[..split]);
    assert!(active.screen().alternate);
    assert!(!active.modes().cursor_visible);
    assert_eq!(active.screen().cell(3, 10).unwrap().glyph(), "雪");
    assert_eq!(active.screen().cell(3, 10).unwrap().width(), 2);
    assert_eq!(active.screen().cell(3, 11).unwrap().width(), 0);
    assert_eq!(active.screen().cell(3, 12).unwrap().glyph(), "🙂");
    assert_eq!(active.screen().cell(3, 14).unwrap().glyph(), "e\u{301}");
    assert!(active.image().unwrap().bytes().len() <= MAX_IMAGE_BYTES);
}
#[test]
fn every_stream_prefix_image_restores_hidden_buffers_modes_and_partial_parser() {
    let bytes = include_bytes!("fixtures/synthetic-cli.ansi");
    for split in 0..=bytes.len() {
        let mut original = renderer(48, 12);
        if split > 0 {
            feed(&mut original, &bytes[..split]);
        }
        let image = original.image().unwrap();
        let mut restored = Renderer::restore(&terminal(), image.digest(), image.bytes()).unwrap();
        assert_eq!(
            restored.image().unwrap().bytes(),
            image.bytes(),
            "prefix {split}"
        );
        if split < bytes.len() {
            feed(&mut original, &bytes[split..]);
            feed(&mut restored, &bytes[split..]);
        }
        assert_eq!(
            restored.image().unwrap().bytes(),
            original.image().unwrap().bytes(),
            "continued prefix {split}"
        );
        assert_eq!(line(&restored, 2), "worker: completed");
    }
}
#[test]
fn resize_is_explicit_ordered_clip_profile_not_inferred_reflow() {
    let mut r = renderer(4, 3);
    feed(&mut r, "ab雪".as_bytes());
    assert!(r.screen().cursor().pending_wrap);
    r.resize(OrderedResize {
        ordinal: 2,
        after_output_sequence: 1,
        columns: 3,
        rows: 2,
    })
    .unwrap();
    assert_eq!(line(&r, 0), "ab");
    assert_eq!(r.screen().cell(0, 2).unwrap().width(), 1);
    let image = r.image().unwrap();
    let mut restored = Renderer::restore(&terminal(), image.digest(), image.bytes()).unwrap();
    feed(&mut r, b"Z\r\nnext");
    feed(&mut restored, b"Z\r\nnext");
    assert_eq!(
        r.image().unwrap().bytes(),
        restored.image().unwrap().bytes()
    );
    assert_eq!(r.resize_generation(), 1);
    let mut partial = renderer(6, 3);
    feed(&mut partial, b"\x1b[31");
    partial
        .resize(OrderedResize {
            ordinal: 2,
            after_output_sequence: 1,
            columns: 4,
            rows: 2,
        })
        .unwrap();
    let image = partial.image().unwrap();
    let mut resumed = Renderer::restore(&terminal(), image.digest(), image.bytes()).unwrap();
    feed(&mut resumed, "m色".as_bytes());
    assert_eq!(
        resumed.screen().cell(0, 0).unwrap().attributes().foreground,
        Color::Indexed(1)
    );
}
#[test]
fn identity_order_digest_and_gap_are_permanent_refusals_not_resets() {
    let mut r = renderer(8, 3);
    let mut changed = frame(1, b"safe");
    changed.digest = Digest::of(b"changed");
    assert_eq!(r.bytes(1, &changed), Err(Error::Order));
    assert!(!r.is_consistent());
    assert!(matches!(r.image(), Err(Error::Inconsistent)));
    assert_eq!(r.bytes(1, &frame(1, b"safe")), Err(Error::Inconsistent));
    let mut r = renderer(8, 3);
    feed(&mut r, b"one");
    assert_eq!(r.bytes(2, &frame(1, b"one")), Err(Error::Order));
    let mut r = renderer(8, 3);
    let mut wrong = frame(1, b"one");
    wrong.terminal.epoch = 2;
    assert_eq!(r.bytes(1, &wrong), Err(Error::Order));
    let mut r = renderer(8, 3);
    assert_eq!(
        r.resize(OrderedResize {
            ordinal: 1,
            after_output_sequence: 1,
            columns: 8,
            rows: 3
        }),
        Err(Error::Order)
    );
    let mut r = renderer(8, 3);
    r.gap();
    assert!(matches!(r.image(), Err(Error::Inconsistent)));
}
#[test]
fn unsupported_compound_unicode_and_controls_never_silently_drop_state() {
    for bytes in [
        "👩\u{200d}💻".as_bytes(),
        "❤\u{fe0f}".as_bytes(),
        "🇦🇺".as_bytes(),
        "👍🏻".as_bytes(),
        b"\x1b[?2026h",
        b"\x1b]52;c;clipboard\x07",
        b"\x1bPgraphics\x1b\\",
    ] {
        let mut r = renderer(20, 4);
        assert!(matches!(
            r.bytes(1, &frame(1, bytes)),
            Err(Error::Unsupported(_))
        ));
        assert!(!r.is_consistent());
        assert!(r.image().is_err());
    }
    let mut narrow = renderer(1, 1);
    assert!(matches!(
        narrow.bytes(1, &frame(1, "雪".as_bytes())),
        Err(Error::Unsupported(_))
    ));
    let mut marks = renderer(8, 2);
    assert!(matches!(
        marks.bytes(
            1,
            &frame(1, format!("e{}", "\u{301}".repeat(32)).as_bytes())
        ),
        Err(Error::Bound)
    ));
}
#[test]
fn utf8_and_control_bounds_fail_before_unbounded_work_or_replacement() {
    for bytes in [b"\xe0\x80".as_slice(), b"\xf4\xbf", b"\xff", b"\xc2x"] {
        let mut r = renderer(8, 2);
        assert_eq!(r.bytes(1, &frame(1, bytes)), Err(Error::Framing));
    }
    let mut r = renderer(8, 2);
    let input = format!("\x1b[{}m", "1".repeat(MAX_CSI_BYTES + 1));
    assert!(r.bytes(1, &frame(1, input.as_bytes())).is_err());
    let mut r = renderer(8, 2);
    let input = format!("\x1b]2;{}", "x".repeat(MAX_CONTROL_BYTES));
    assert!(r.bytes(1, &frame(1, input.as_bytes())).is_err());
    let mut r = renderer(8, 2);
    assert_eq!(r.bytes(1, &frame(1, b"a\x1b[65535b")), Err(Error::Bound));
    assert!(
        Renderer::from_assumed_blank(AssumedBlank {
            terminal: terminal(),
            columns: 240,
            rows: 51
        })
        .is_err()
    );
}
#[test]
fn malformed_or_cross_terminal_images_are_bounded_and_cannot_continue() {
    let mut r = renderer(12, 4);
    feed(
        &mut r,
        "a雪e\u{301}\x1b[31m\x1b[?1049hhidden\x1b[4;2H".as_bytes(),
    );
    let image = r.image().unwrap();
    for length in 0..image.bytes().len() {
        assert!(
            Renderer::restore(
                &terminal(),
                &Digest::of(&image.bytes()[..length]),
                &image.bytes()[..length]
            )
            .is_err()
        );
    }
    let mut other = terminal();
    other.epoch = 2;
    assert!(Renderer::restore(&other, image.digest(), image.bytes()).is_err());
    let mut trailing = image.bytes().to_vec();
    trailing.push(0);
    assert!(Renderer::restore(&terminal(), &Digest::of(&trailing), &trailing).is_err());
    for at in 0..image.bytes().len() {
        let mut mutated = image.bytes().to_vec();
        mutated[at] = 255;
        let outcome = std::panic::catch_unwind(|| {
            Renderer::restore(&terminal(), &Digest::of(&mutated), &mutated)
        });
        assert!(outcome.is_ok(), "malformed codec panic at {at}");
    }
}
#[test]
fn configured_history_retention_is_finite_and_explicit() {
    let mut r = renderer(1, 1);
    let bytes = b"a\r\n".repeat(MAX_HISTORY_CELLS + 10);
    feed(&mut r, &bytes);
    assert_eq!(
        r.screen()
            .history()
            .map(|(cells, _)| cells.len())
            .sum::<usize>(),
        MAX_HISTORY_CELLS
    );
    assert!(r.is_consistent());
}

#[test]
fn reset_and_alternate_reallocation_are_charged_before_work_expansion() {
    for pattern in [b"\x1bc".as_slice(), b"\x1b[?1049h", b"\x1b[2J"] {
        let mut r = renderer(240, 50);
        let bytes = pattern.repeat(200);
        assert_eq!(r.bytes(1, &frame(1, &bytes)), Err(Error::Bound));
        assert!(!r.is_consistent());
        assert!(r.image().is_err());
    }
}
#[test]
fn large_valid_owned_state_refuses_image_instead_of_truncating_cells() {
    let mut bytes = Vec::new();
    for alternate in [false, true] {
        if alternate {
            bytes.extend_from_slice(b"\x1b[?1049h");
        }
        for n in 0..MAX_GRID_CELLS {
            bytes.extend_from_slice(
                format!("\x1b[38;2;{};{};{}mX", n % 256, (n / 256) % 256, n % 251).as_bytes(),
            );
        }
    }
    let mut r = renderer(240, 50);
    for chunk in bytes.chunks(16384) {
        feed(&mut r, chunk);
    }
    assert!(r.is_consistent());
    assert!(matches!(r.image(), Err(Error::Bound)));
    assert_eq!(r.screen().cell(49, 239).unwrap().glyph(), "X");
}
#[test]
fn cursor_attributes_tabs_margins_and_cell_editing_have_independent_expected_states() {
    let mut r = renderer(16, 5);
    feed(&mut r, b"abcdef\r\x1b[2C\x1b[2P");
    assert_eq!(line(&r, 0), "abef");
    feed(&mut r, b"\x1b[2@XY");
    assert_eq!(line(&r, 0), "abXYef");
    feed(&mut r, b"\x1b[1;4;38;5;200;48;2;1;2;3mZ");
    let attrs = r.screen().cell(0, 4).unwrap().attributes();
    assert_eq!(attrs.foreground, Color::Indexed(200));
    assert_eq!(attrs.background, Color::Rgb(1, 2, 3));
    assert_eq!(attrs.flags, 9);
    feed(&mut r, b"\x1b7\x1b[0m\x1b[4;4Hbad\x1b8Q");
    assert_eq!(r.screen().cell(0, 5).unwrap().glyph(), "Q");
    assert_eq!(r.screen().cell(0, 5).unwrap().attributes(), attrs);
    feed(&mut r, b"\x1b[1;1H\t!");
    assert_eq!(r.screen().cell(0, 8).unwrap().glyph(), "!");
    feed(&mut r, b"\x1b[2;4r\x1b[?6h\x1b[1;1Hregion");
    assert_eq!(r.screen().cell(1, 0).unwrap().glyph(), "r");
    feed(&mut r, b"\x1b[?6l\x1b[0;0r\x1b[5;1Hbottom");
    assert_eq!(line(&r, 4), "bottom");
    feed(&mut r, b"\x1b[0m\x1b[3;1H\x1b(0lqk\x1b(B");
    assert_eq!(line(&r, 2), "┌─┐");
}

#[test]
fn overlapping_mouse_and_underline_modes_preserve_fallback_state() {
    let mut r = renderer(8, 2);
    feed(
        &mut r,
        b"\x1b[?1000h\x1b[?1002h\x1b[?1005h\x1b[?1006h\x1b[?1006l\x1b[?1002l",
    );
    assert_eq!(r.modes().mouse_modes, 1);
    assert_eq!(r.modes().mouse_encodings, 1);
    let image = r.image().unwrap();
    let mut restored = Renderer::restore(&terminal(), image.digest(), image.bytes()).unwrap();
    feed(&mut r, b"\x1b[?1000l\x1b[?1005l\x1b[21mA\x1b[4mB");
    feed(&mut restored, b"\x1b[?1000l\x1b[?1005l\x1b[21mA\x1b[4mB");
    assert_eq!(
        r.image().unwrap().bytes(),
        restored.image().unwrap().bytes()
    );
    assert_eq!(r.modes().mouse_modes, 0);
    assert_eq!(r.screen().cell(0, 0).unwrap().attributes().flags, 256);
    assert_eq!(r.screen().cell(0, 1).unwrap().attributes().flags, 8);
}

#[test]
fn frozen_binary_vector_preserves_both_screens_and_partial_colour_parser() {
    let bytes = include_bytes!("fixtures/alternate-partial.aqri");
    let mut restored = Renderer::restore(&terminal(), &Digest::of(bytes), bytes).unwrap();
    assert!(restored.screen().alternate);
    assert!(!restored.modes().cursor_visible);
    assert!(restored.modes().application_cursor);
    assert!(restored.modes().bracketed_paste);
    assert_eq!(restored.image().unwrap().bytes(), bytes);
    feed(&mut restored, b"5;6m!");
    let cursor = restored.screen().cursor();
    assert_eq!(
        restored
            .screen()
            .cell(cursor.row as usize, cursor.column as usize - 1)
            .unwrap()
            .attributes()
            .foreground,
        Color::Rgb(4, 5, 6)
    );
    feed(&mut restored, b"\x1b[?1049l");
    assert_eq!(line(&restored, 0), "worker: queued");
    let mut changed = bytes.to_vec();
    changed[7] ^= 1;
    assert!(Renderer::restore(&terminal(), &Digest::of(&changed), &changed).is_err());
    assert!(Renderer::restore(&terminal(), &Digest::of(b"wrong content address"), bytes).is_err());
}

#[test]
fn title_icon_and_partial_utf8_osc_state_survive_codec_without_control_effects() {
    let stream = "\x1b]0;both 雪\x07\x1b]2;title🙂\x1b\\\x1b]1;icon\x07".as_bytes();
    for split in 1..stream.len() {
        let mut r = renderer(8, 2);
        feed(&mut r, &stream[..split]);
        let image = r.image().unwrap();
        let mut resumed = Renderer::restore(&terminal(), image.digest(), image.bytes()).unwrap();
        feed(&mut resumed, &stream[split..]);
        assert_eq!(resumed.title(), "title🙂");
        assert_eq!(resumed.icon_name(), "icon");
        assert_eq!(line(&resumed, 0), "");
    }
}
#[test]
fn retained_vector_hashes_drift_only_with_an_explicit_fixture_update() {
    let expected = format!(
        "{}  alternate-partial.aqri\n{}  synthetic-cli.ansi\n",
        Digest::of(include_bytes!("fixtures/alternate-partial.aqri")).as_str(),
        Digest::of(include_bytes!("fixtures/synthetic-cli.ansi")).as_str()
    );
    assert_eq!(include_str!("fixtures/vectors.sha256"), expected);
}

#[test]
fn rep_after_combining_refuses_instead_of_disagreeing_with_actual_tmux_oracle() {
    let mut ascii = renderer(20, 4);
    feed(&mut ascii, b"\x1b[2J\x1b[HA\x1b[3b|");
    assert_eq!(line(&ascii, 0), "AAAA|");
    let mut combining = renderer(20, 4);
    feed(&mut combining, "\x1b[2J\x1b[He\u{301}".as_bytes());
    let image = combining.image().unwrap();
    let mut restored = Renderer::restore(&terminal(), image.digest(), image.bytes()).unwrap();
    for model in [&mut combining, &mut restored] {
        let result = model.bytes(
            model.through_ordinal() + 1,
            &frame(model.through_sequence() + 1, b"\x1b[3b|"),
        );
        assert!(matches!(result, Err(Error::Unsupported(_))));
        assert!(!model.is_consistent());
        assert!(model.image().is_err());
        assert_eq!(line(model, 0), "e\u{301}");
    }
}
