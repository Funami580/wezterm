#![allow(clippy::many_single_char_names)]
use crate::escape::{
    Action, DeviceControlMode, EnterDeviceControlMode, Esc, OperatingSystemCommand,
    ShortDeviceControl, CSI,
};
use crate::tmux_cc::Event;
use log::error;
use num_traits::FromPrimitive;
use std::borrow::BorrowMut;
use std::cell::RefCell;
use vtparse::{CsiParam, VTActor, VTParser};

mod sixel;
use sixel::SixelBuilder;

#[derive(Default)]
struct GetTcapBuilder {
    current: Vec<u8>,
    names: Vec<String>,
}

impl GetTcapBuilder {
    fn flush(&mut self) {
        let decoded = hex::decode(&self.current)
            .map(|s| String::from_utf8_lossy(&s).to_string())
            .unwrap_or_else(|_| String::from_utf8_lossy(&self.current).to_string());
        self.names.push(decoded);
        self.current.clear();
    }

    pub fn push(&mut self, data: u8) {
        if data == b';' {
            self.flush();
        } else {
            self.current.push(data);
        }
    }

    pub fn finish(mut self) -> Vec<String> {
        self.flush();
        self.names
    }
}

#[derive(Default)]
struct ParseState {
    sixel: Option<SixelBuilder>,
    dcs: Option<ShortDeviceControl>,
    get_tcap: Option<GetTcapBuilder>,
    tmux_state: Option<RefCell<crate::tmux_cc::Parser>>,
}

/// The `Parser` struct holds the state machine that is used to decode
/// a sequence of bytes.  The byte sequence can be streaming into the
/// state machine.
/// You can either have the parser trigger a callback as `Action`s are
/// decoded, or have it return a `Vec<Action>` holding zero-or-more
/// decoded actions.
pub struct Parser {
    state_machine: VTParser,
    state: RefCell<ParseState>,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Self {
            state_machine: VTParser::new(),
            state: RefCell::new(Default::default()),
        }
    }

    /// advance with tmux parser, bypass VTParse
    fn advance_tmux_bytes(&mut self, bytes: &[u8]) -> anyhow::Result<Vec<Event>> {
        let parser_state = self.state.borrow();
        let tmux_state = parser_state.tmux_state.as_ref().unwrap();
        let mut tmux_parser = tmux_state.borrow_mut();
        return tmux_parser.advance_bytes(bytes);
    }

    pub fn parse<F: FnMut(Action)>(&mut self, bytes: &[u8], mut callback: F) {
        let is_tmux_mode: bool = self.state.borrow().tmux_state.is_some();
        if is_tmux_mode {
            match self.advance_tmux_bytes(bytes) {
                Ok(tmux_events) => {
                    callback(Action::DeviceControl(DeviceControlMode::TmuxEvents(
                        Box::new(tmux_events),
                    )));
                }
                Err(err_buf) => {
                    // capture bytes cannot be parsed
                    let unparsed_str = err_buf.to_string().to_owned();
                    let mut parser_state = self.state.borrow_mut();
                    parser_state.tmux_state = None;
                    let mut perform = Performer {
                        callback: &mut callback,
                        state: &mut parser_state,
                    };
                    self.state_machine
                        .parse(unparsed_str.as_bytes(), &mut perform);
                }
            }
        } else {
            let mut perform = Performer {
                callback: &mut callback,
                state: &mut self.state.borrow_mut(),
            };
            self.state_machine.parse(bytes, &mut perform);
        }
    }

    /// A specialized version of the parser that halts after recognizing the
    /// first action from the stream of bytes.  The return value is the action
    /// that was recognized and the length of the byte stream that was fed in
    /// to the parser to yield it.
    pub fn parse_first(&mut self, bytes: &[u8]) -> Option<(Action, usize)> {
        // holds the first action.  We need to use RefCell to deal with
        // the Performer holding a reference to this via the closure we set up.
        let first = RefCell::new(None);
        // will hold the iterator index when we emit an action
        let mut first_idx = None;
        {
            let mut perform = Performer {
                callback: &mut |action| {
                    // capture the action, but only if it is the first one
                    // we've seen.  Preserve an existing one if any.
                    if first.borrow().is_some() {
                        return;
                    }
                    *first.borrow_mut() = Some(action);
                },
                state: &mut self.state.borrow_mut(),
            };
            for (idx, b) in bytes.iter().enumerate() {
                self.state_machine.parse_byte(*b, &mut perform);
                if first.borrow().is_some() {
                    // if we recognized an action, record the iterator index
                    first_idx = Some(idx);
                    break;
                }
            }
        }

        match (first.into_inner(), first_idx) {
            // if we matched an action, transform the iterator index to
            // the length of the string that was consumed (+1)
            (Some(action), Some(idx)) => Some((action, idx + 1)),
            _ => None,
        }
    }

    pub fn parse_as_vec(&mut self, bytes: &[u8]) -> Vec<Action> {
        let mut result = Vec::new();
        self.parse(bytes, |action| result.push(action));
        result
    }

    /// Similar to `parse_first` but collects all actions from the first sequence.
    pub fn parse_first_as_vec(&mut self, bytes: &[u8]) -> Option<(Vec<Action>, usize)> {
        let mut actions = Vec::new();
        let mut first_idx = None;
        for (idx, b) in bytes.iter().enumerate() {
            self.state_machine.parse_byte(
                *b,
                &mut Performer {
                    callback: &mut |action| actions.push(action),
                    state: &mut self.state.borrow_mut(),
                },
            );
            if !actions.is_empty() {
                // if we recognized any actions, record the iterator index
                first_idx = Some(idx);
                break;
            }
        }
        first_idx.map(|idx| (actions, idx + 1))
    }
}

struct Performer<'a, F: FnMut(Action) + 'a> {
    callback: &'a mut F,
    state: &'a mut ParseState,
}

fn is_short_dcs(intermediates: &[u8], byte: u8) -> bool {
    if intermediates == &[b'$'] && byte == b'q' {
        // DECRQSS
        true
    } else {
        false
    }
}

impl<'a, F: FnMut(Action)> VTActor for Performer<'a, F> {
    fn print(&mut self, c: char) {
        (self.callback)(Action::Print(c));
    }

    fn execute_c0_or_c1(&mut self, byte: u8) {
        match FromPrimitive::from_u8(byte) {
            Some(code) => (self.callback)(Action::Control(code)),
            None => error!(
                "impossible C0/C1 control code {:?} 0x{:x} was dropped",
                byte as char, byte
            ),
        }
    }

    fn apc_dispatch(&mut self, data: Vec<u8>) {
        if let Some(img) = super::KittyImage::parse_apc(&data) {
            (self.callback)(Action::KittyImage(img))
        } else {
            log::trace!("Ignoring APC data: {:?}", String::from_utf8_lossy(&data));
        }
    }

    fn dcs_hook(
        &mut self,
        byte: u8,
        params: &[i64],
        intermediates: &[u8],
        ignored_extra_intermediates: bool,
    ) {
        self.state.sixel.take();
        self.state.get_tcap.take();
        self.state.dcs.take();
        if byte == b'q' && intermediates.is_empty() && !ignored_extra_intermediates {
            self.state.sixel.replace(SixelBuilder::new(params));
        } else if byte == b'q' && intermediates == [b'+'] {
            self.state.get_tcap.replace(GetTcapBuilder::default());
        } else if !ignored_extra_intermediates && is_short_dcs(intermediates, byte) {
            self.state.dcs.replace(ShortDeviceControl {
                params: params.to_vec(),
                intermediates: intermediates.to_vec(),
                byte,
                data: vec![],
            });
        } else {
            if byte == b'p' && params == [1000] {
                // into tmux_cc mode
                self.state.borrow_mut().tmux_state =
                    Some(RefCell::new(crate::tmux_cc::Parser::new()));
            }
            (self.callback)(Action::DeviceControl(DeviceControlMode::Enter(Box::new(
                EnterDeviceControlMode {
                    byte,
                    params: params.to_vec(),
                    intermediates: intermediates.to_vec(),
                    ignored_extra_intermediates,
                },
            ))));
        }
    }

    fn dcs_put(&mut self, data: u8) {
        if let Some(dcs) = self.state.dcs.as_mut() {
            dcs.data.push(data);
        } else if let Some(sixel) = self.state.sixel.as_mut() {
            sixel.push(data);
        } else if let Some(tcap) = self.state.get_tcap.as_mut() {
            tcap.push(data);
        } else {
            if let Some(tmux_state) = &self.state.tmux_state {
                let mut tmux_parser = tmux_state.borrow_mut();
                match tmux_parser.advance_byte(data) {
                    Ok(optional_events) => {
                        if let Some(tmux_event) = optional_events {
                            (self.callback)(Action::DeviceControl(DeviceControlMode::TmuxEvents(
                                Box::new(vec![tmux_event]),
                            )));
                        }
                    }
                    Err(_) => {
                        drop(tmux_parser);
                        self.state.tmux_state = None; // drop tmux state
                    }
                }
            } else {
                (self.callback)(Action::DeviceControl(DeviceControlMode::Data(data)));
            }
        }
    }

    fn dcs_unhook(&mut self) {
        if let Some(dcs) = self.state.dcs.take() {
            (self.callback)(Action::DeviceControl(
                DeviceControlMode::ShortDeviceControl(Box::new(dcs)),
            ));
        } else if let Some(mut sixel) = self.state.sixel.take() {
            sixel.finish();
            (self.callback)(Action::Sixel(Box::new(sixel.sixel)));
        } else if let Some(tcap) = self.state.get_tcap.take() {
            (self.callback)(Action::XtGetTcap(tcap.finish()));
        } else {
            (self.callback)(Action::DeviceControl(DeviceControlMode::Exit));
        }
    }

    fn osc_dispatch(&mut self, osc: &[&[u8]]) {
        let osc = OperatingSystemCommand::parse(osc);
        (self.callback)(Action::OperatingSystemCommand(Box::new(osc)));
    }

    fn csi_dispatch(&mut self, params: &[CsiParam], parameters_truncated: bool, control: u8) {
        for action in CSI::parse(params, parameters_truncated, control as char) {
            (self.callback)(Action::CSI(action));
        }
    }

    fn esc_dispatch(
        &mut self,
        _params: &[i64],
        intermediates: &[u8],
        _ignored_extra_intermediates: bool,
        control: u8,
    ) {
        // It doesn't appear to be possible for params.len() > 1 due to the way
        // that the state machine in vte functions.  As such, it also seems to
        // be impossible for ignored_extra_intermediates to be true too.
        (self.callback)(Action::Esc(Esc::parse(
            if intermediates.len() == 1 {
                Some(intermediates[0])
            } else {
                None
            },
            control,
        )));
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::cell::{Intensity, Underline};
    use crate::color::{ColorSpec, RgbColor};
    use crate::escape::csi::{
        CharacterPath, DecPrivateMode, DecPrivateModeCode, Device, Mode, Sgr, Window, XtSmGraphics,
        XtSmGraphicsItem, XtermKeyModifierResource,
    };
    use crate::escape::{EscCode, OneBased};
    use k9::assert_equal as assert_eq;
    use std::io::Write;

    fn encode(seq: &Vec<Action>) -> String {
        let mut res = Vec::new();
        for s in seq {
            write!(res, "{}", s).unwrap();
        }
        String::from_utf8(res).unwrap()
    }

    #[test]
    fn basic_parse() {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(b"hello");
        assert_eq!(
            vec![
                Action::Print('h'),
                Action::Print('e'),
                Action::Print('l'),
                Action::Print('l'),
                Action::Print('o'),
            ],
            actions
        );
        assert_eq!(encode(&actions), "hello");
    }

    #[test]
    fn basic_bold() {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(b"\x1b[1mb");
        assert_eq!(
            vec![
                Action::CSI(CSI::Sgr(Sgr::Intensity(Intensity::Bold))),
                Action::Print('b'),
            ],
            actions
        );
        assert_eq!(encode(&actions), "\x1b[1mb");
    }

    #[test]
    fn basic_bold_italic() {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(b"\x1b[1;3mb");
        assert_eq!(
            vec![
                Action::CSI(CSI::Sgr(Sgr::Intensity(Intensity::Bold))),
                Action::CSI(CSI::Sgr(Sgr::Italic(true))),
                Action::Print('b'),
            ],
            actions
        );

        assert_eq!(encode(&actions), "\x1b[1m\x1b[3mb");
    }

    #[test]
    fn fancy_underline() {
        let mut p = Parser::new();

        let actions = p.parse_as_vec(b"\x1b[4:0;4:1;4:2;4:3;4:4;4:5mb");
        assert_eq!(
            vec![
                Action::CSI(CSI::Sgr(Sgr::Underline(Underline::None))),
                Action::CSI(CSI::Sgr(Sgr::Underline(Underline::Single))),
                Action::CSI(CSI::Sgr(Sgr::Underline(Underline::Double))),
                Action::CSI(CSI::Sgr(Sgr::Underline(Underline::Curly))),
                Action::CSI(CSI::Sgr(Sgr::Underline(Underline::Dotted))),
                Action::CSI(CSI::Sgr(Sgr::Underline(Underline::Dashed))),
                Action::Print('b'),
            ],
            actions
        );

        assert_eq!(
            encode(&actions),
            "\x1b[24m\x1b[4m\x1b[21m\x1b[4:3m\x1b[4:4m\x1b[4:5mb"
        );
    }

    #[test]
    fn true_color() {
        let mut p = Parser::new();

        let actions = p.parse_as_vec(b"\x1b[38:2::128:64:192mw");
        assert_eq!(
            vec![
                Action::CSI(CSI::Sgr(Sgr::Foreground(ColorSpec::TrueColor(
                    RgbColor::new_8bpc(128, 64, 192)
                )))),
                Action::Print('w'),
            ],
            actions
        );

        assert_eq!(encode(&actions), "\u{1b}[38:2::128:64:192mw");

        let actions = p.parse_as_vec(b"\x1b[38:2:0:255:0mw");
        assert_eq!(
            vec![
                Action::CSI(CSI::Sgr(Sgr::Foreground(ColorSpec::TrueColor(
                    RgbColor::new_8bpc(0, 255, 0)
                )))),
                Action::Print('w'),
            ],
            actions
        );
    }

    #[test]
    fn basic_osc() {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(b"\x1b]0;hello\x07");
        assert_eq!(
            vec![Action::OperatingSystemCommand(Box::new(
                OperatingSystemCommand::SetIconNameAndWindowTitle("hello".to_owned()),
            ))],
            actions
        );
        assert_eq!(encode(&actions), "\x1b]0;hello\x1b\\");

        let actions = p.parse_as_vec(b"\x1b]532534523;hello\x07");
        assert_eq!(
            vec![Action::OperatingSystemCommand(Box::new(
                OperatingSystemCommand::Unspecified(vec![b"532534523".to_vec(), b"hello".to_vec()]),
            ))],
            actions
        );
        assert_eq!(encode(&actions), "\x1b]532534523;hello\x1b\\");
    }

    #[test]
    fn test_emoji_title_osc() {
        let input = "\x1b]0;\u{1f915}\x07";
        let mut p = Parser::new();
        let actions = p.parse_as_vec(input.as_bytes());
        assert_eq!(
            vec![Action::OperatingSystemCommand(Box::new(
                OperatingSystemCommand::SetIconNameAndWindowTitle("\u{1f915}".to_owned()),
            ))],
            actions
        );
        assert_eq!(encode(&actions), "\x1b]0;\u{1f915}\x1b\\");
    }

    #[test]
    fn basic_esc() {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(b"\x1bH");
        assert_eq!(
            vec![Action::Esc(Esc::Code(EscCode::HorizontalTabSet))],
            actions
        );
        assert_eq!(encode(&actions), "\x1bH");

        let actions = p.parse_as_vec(b"\x1b%H");
        assert_eq!(
            vec![Action::Esc(Esc::Unspecified {
                intermediate: Some(b'%'),
                control: b'H',
            })],
            actions
        );
        assert_eq!(encode(&actions), "\x1b%H");
    }

    #[test]
    fn soft_reset() {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(b"\x1b[!p");
        assert_eq!(
            vec![Action::CSI(CSI::Device(Box::new(
                crate::escape::csi::Device::SoftReset
            )))],
            actions
        );
        assert_eq!(encode(&actions), "\x1b[!p");
    }

    #[test]
    fn tmux_title_escape() {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(b"\x1bktitle\x1b\\");
        assert_eq!(
            vec![
                Action::Esc(Esc::Code(EscCode::TmuxTitle)),
                Action::Print('t'),
                Action::Print('i'),
                Action::Print('t'),
                Action::Print('l'),
                Action::Print('e'),
                Action::Esc(Esc::Code(EscCode::StringTerminator)),
            ],
            actions
        );
    }

    fn round_trip_parse(s: &str) -> Vec<Action> {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(s.as_bytes());
        println!("actions: {:?}", actions);
        assert_eq!(s, encode(&actions));
        actions
    }

    fn parse_as(s: &str, expected: &str) -> Vec<Action> {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(s.as_bytes());
        println!("actions: {:?}", actions);
        assert_eq!(expected, encode(&actions));
        actions
    }

    #[test]
    fn xtgettcap() {
        assert_eq!(
            round_trip_parse("\x1bP+q544e\x1b\\"),
            vec![
                Action::XtGetTcap(vec!["TN".to_string()]),
                Action::Esc(Esc::Code(EscCode::StringTerminator)),
            ]
        );
    }

    #[test]
    fn bidi_modes() {
        assert_eq!(
            round_trip_parse("\x1b[1 k"),
            vec![Action::CSI(CSI::SelectCharacterPath(
                CharacterPath::LeftToRightOrTopToBottom,
                0
            ))]
        );
        assert_eq!(
            round_trip_parse("\x1b[2;1 k"),
            vec![Action::CSI(CSI::SelectCharacterPath(
                CharacterPath::RightToLeftOrBottomToTop,
                1
            ))]
        );
    }

    #[test]
    fn xterm_key() {
        assert_eq!(
            round_trip_parse("\x1b[>4;2m"),
            vec![Action::CSI(CSI::Mode(Mode::XtermKeyMode {
                resource: XtermKeyModifierResource::OtherKeys,
                value: Some(2),
            }))]
        );
    }

    #[test]
    fn window() {
        assert_eq!(
            round_trip_parse("\x1b[22;2t"),
            vec![Action::CSI(CSI::Window(Window::PushWindowTitle))]
        );
    }

    #[test]
    fn checksum_area() {
        assert_eq!(
            round_trip_parse("\x1b[1;2;3;4;5;6*y"),
            vec![Action::CSI(CSI::Window(Window::ChecksumRectangularArea {
                request_id: 1,
                page_number: 2,
                top: OneBased::new(3),
                left: OneBased::new(4),
                bottom: OneBased::new(5),
                right: OneBased::new(6),
            }))]
        );
    }

    #[test]
    fn dec_private_modes() {
        assert_eq!(
            parse_as("\x1b[?1;1006h", "\x1b[?1h\x1b[?1006h"),
            vec![
                Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                    DecPrivateModeCode::ApplicationCursorKeys
                ),))),
                Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                    DecPrivateModeCode::SGRMouse
                ),))),
            ]
        );
    }

    #[test]
    fn xtsmgraphics() {
        assert_eq!(
            round_trip_parse("\x1b[?1;3;256S"),
            vec![Action::CSI(CSI::Device(Box::new(Device::XtSmGraphics(
                XtSmGraphics {
                    item: XtSmGraphicsItem::NumberOfColorRegisters,
                    action_or_status: 3,
                    value: vec![256]
                }
            ))))]
        );
    }

    #[test]
    fn req_attr() {
        assert_eq!(
            round_trip_parse("\x1b[=c"),
            vec![Action::CSI(CSI::Device(Box::new(
                Device::RequestTertiaryDeviceAttributes
            )))]
        );
        assert_eq!(
            round_trip_parse("\x1b[>c"),
            vec![Action::CSI(CSI::Device(Box::new(
                Device::RequestSecondaryDeviceAttributes
            )))]
        );
    }

    #[test]
    fn sgr() {
        assert_eq!(
            parse_as("\x1b[;4m", "\x1b[0m\x1b[4m"),
            vec![
                Action::CSI(CSI::Sgr(Sgr::Reset)),
                Action::CSI(CSI::Sgr(Sgr::Underline(Underline::Single))),
            ]
        );
    }

    #[test]
    fn kitty_img() {
        use crate::escape::apc::*;
        assert_eq!(
            round_trip_parse("\x1b_Gf=24,s=10,v=20;aGVsbG8=\x1b\\"),
            vec![
                Action::KittyImage(KittyImage::TransmitData {
                    transmit: KittyImageTransmit {
                        format: Some(KittyImageFormat::Rgb),
                        data: KittyImageData::Direct("aGVsbG8=".to_string()),
                        width: Some(10),
                        height: Some(20),
                        image_id: None,
                        image_number: None,
                        compression: KittyImageCompression::None,
                        more_data_follows: false,
                    },
                    verbosity: KittyImageVerbosity::Verbose,
                }),
                Action::Esc(Esc::Code(EscCode::StringTerminator)),
            ]
        );

        assert_eq!(
            parse_as(
                "\x1b_Ga=q,s=1,v=1,i=1;YWJjZA==\x1b\\",
                "\x1b_Ga=q,i=1,s=1,v=1;YWJjZA==\x1b\\"
            ),
            vec![
                Action::KittyImage(KittyImage::Query {
                    transmit: KittyImageTransmit {
                        format: None,
                        data: KittyImageData::Direct("YWJjZA==".to_string()),
                        width: Some(1),
                        height: Some(1),
                        image_id: Some(1),
                        image_number: None,
                        compression: KittyImageCompression::None,
                        more_data_follows: false,
                    },
                }),
                Action::Esc(Esc::Code(EscCode::StringTerminator)),
            ]
        );
        assert_eq!(
            parse_as(
                "\x1b_Ga=q,t=f,s=1,v=1,i=2;L3Zhci90bXAvdG1wdGYxd3E4Ym4=\x1b\\",
                "\x1b_Ga=q,i=2,s=1,t=f,v=1;L3Zhci90bXAvdG1wdGYxd3E4Ym4=\x1b\\"
            ),
            vec![
                Action::KittyImage(KittyImage::Query {
                    transmit: KittyImageTransmit {
                        format: None,
                        data: KittyImageData::File {
                            path: "/var/tmp/tmptf1wq8bn".to_string(),
                            data_offset: None,
                            data_size: None,
                        },
                        width: Some(1),
                        height: Some(1),
                        image_id: Some(2),
                        image_number: None,
                        compression: KittyImageCompression::None,
                        more_data_follows: false,
                    },
                }),
                Action::Esc(Esc::Code(EscCode::StringTerminator)),
            ]
        );
    }

    #[test]
    fn decset() {
        assert_eq!(
            round_trip_parse("\x1b[?23434h"),
            vec![Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(
                DecPrivateMode::Unspecified(23434),
            )))]
        );

        /*
        {
            let res = CSI::parse(&[CsiParam::Integer(2026)], &[b'?', b'$'], false, 'p').collect();
            assert_eq!(encode(&res), "\x1b[?2026$p");
        }
        */

        assert_eq!(
            round_trip_parse("\x1b[?1l"),
            vec![Action::CSI(CSI::Mode(Mode::ResetDecPrivateMode(
                DecPrivateMode::Code(DecPrivateModeCode::ApplicationCursorKeys,)
            )))]
        );

        assert_eq!(
            round_trip_parse("\x1b[?25s"),
            vec![Action::CSI(CSI::Mode(Mode::SaveDecPrivateMode(
                DecPrivateMode::Code(DecPrivateModeCode::ShowCursor,)
            )))]
        );
        assert_eq!(
            round_trip_parse("\x1b[?2004r"),
            vec![Action::CSI(CSI::Mode(Mode::RestoreDecPrivateMode(
                DecPrivateMode::Code(DecPrivateModeCode::BracketedPaste),
            )))]
        );
        assert_eq!(
            round_trip_parse("\x1b[?12h\x1b[?25h"),
            vec![
                Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                    DecPrivateModeCode::StartBlinkingCursor,
                )))),
                Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                    DecPrivateModeCode::ShowCursor,
                )))),
            ]
        );

        assert_eq!(
            round_trip_parse("\x1b[?1002h\x1b[?1003h\x1b[?1005h\x1b[?1006h"),
            vec![
                Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                    DecPrivateModeCode::ButtonEventMouse,
                )))),
                Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                    DecPrivateModeCode::AnyEventMouse,
                )))),
                Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(
                    DecPrivateMode::Unspecified(1005)
                ))),
                Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                    DecPrivateModeCode::SGRMouse,
                )))),
            ]
        );
    }

    #[test]
    fn issue_1291() {
        use crate::escape::osc::{ITermDimension, ITermFileData, ITermProprietary};

        let mut p = Parser::new();
        // Note the empty k=v pair immediately following `File=`
        let actions = p.parse_as_vec(b"\x1b]1337;File=;size=234:aGVsbG8=\x07");
        assert_eq!(
            vec![Action::OperatingSystemCommand(Box::new(
                OperatingSystemCommand::ITermProprietary(ITermProprietary::File(Box::new(
                    ITermFileData {
                        name: None,
                        size: Some(234),
                        width: ITermDimension::Automatic,
                        height: ITermDimension::Automatic,
                        preserve_aspect_ratio: true,
                        inline: false,
                        do_not_move_cursor: false,
                        data: b"hello".to_vec(),
                    }
                )))
            ))],
            actions
        );
    }
}
