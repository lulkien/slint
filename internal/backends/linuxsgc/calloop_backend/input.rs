// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// cSpell: ignore keystate Keysym RDONLY RDWR
//! This module contains the code to receive input events from libinput. The
//! devices come from the @sgc daemon (grants, see `input_shared`); this
//! module only owns a handle of the shared path context and dispatches the
//! events to the window.

use std::cell::RefCell;
use std::pin::Pin;
use std::rc::Rc;

use i_slint_core::api::LogicalPosition;
use i_slint_core::lengths::logical_point_from_api;
use i_slint_core::platform::{PlatformError, PointerEventButton, WindowEvent};
use i_slint_core::window::{WindowAdapter, WindowInner};
use i_slint_core::{Property, SharedString};
use input::event::keyboard::{KeyState, KeyboardEventTrait};
use input::event::touch::{TouchEventPosition, TouchEventSlot};
use xkbcommon::*;

use super::input_shared::InputState;
use crate::fullscreenwindowadapter::FullscreenWindowAdapter;

pub struct LibInputHandler<'a> {
    /// Our handle of the shared libinput path context (a clone — the
    /// original lives in the [`InputState`]).
    libinput: input::Libinput,
    token: Option<calloop::Token>,
    mouse_pos: Pin<Rc<Property<Option<LogicalPosition>>>>,
    /// Last known position per touch slot. We must track positions because
    /// touch-up events from libinput do not include coordinates — only the slot
    /// identifier is available, so we replay the last known position.
    /// Fixed-capacity to avoid heap allocation — touchscreens rarely report
    /// more than 5 simultaneous contacts.
    last_touch_positions: [(i32, Option<LogicalPosition>); 5],
    window: &'a RefCell<Option<Rc<FullscreenWindowAdapter>>>,
    keystate: Option<xkb::State>,
    libinput_event_hook: &'a Option<Box<dyn Fn(&::input::Event) -> bool>>,
}

impl<'a> LibInputHandler<'a> {
    pub fn init<T>(
        window: &'a RefCell<Option<Rc<FullscreenWindowAdapter>>>,
        event_loop_handle: &calloop::LoopHandle<'a, T>,
        libinput_event_hook: &'a Option<Box<dyn Fn(&::input::Event) -> bool>>,
        input_state: Rc<InputState>,
    ) -> Result<Pin<Rc<Property<Option<LogicalPosition>>>>, PlatformError> {
        // Hand the session's granted devices to libinput. This runs here, on
        // the event-loop thread: libinput is not thread-safe and adding a
        // device opens it synchronously through the interface. (Later grants
        // and revokes arrive through the sgc pump and add/remove devices the
        // same way, on this thread.)
        input_state.add_pending_devices();

        let libinput = input_state.libinput();

        let mouse_pos_property = Rc::pin(Property::new(None));

        let handler = Self {
            libinput,
            token: Default::default(),
            mouse_pos: mouse_pos_property.clone(),
            last_touch_positions: Default::default(),
            window,
            keystate: Default::default(),
            libinput_event_hook,
        };

        event_loop_handle
            .insert_source(handler, move |_, _, _| {})
            .map_err(|e| format!("Error registering libinput event source: {e}"))?;

        Ok(mouse_pos_property)
    }
}

fn set_touch_pos(
    positions: &mut [(i32, Option<LogicalPosition>); 5],
    slot: i32,
    pos: LogicalPosition,
) {
    if let Some(entry) = positions.iter_mut().find(|(s, _)| *s == slot) {
        entry.1 = Some(pos);
    } else if let Some(entry) = positions.iter_mut().find(|(_, p)| p.is_none()) {
        *entry = (slot, Some(pos));
    }
}

fn take_touch_pos(
    positions: &mut [(i32, Option<LogicalPosition>); 5],
    slot: i32,
) -> LogicalPosition {
    positions
        .iter_mut()
        .find(|(s, _)| *s == slot)
        .and_then(|entry| entry.1.take())
        .unwrap_or_default()
}

impl<'a> calloop::EventSource for LibInputHandler<'a> {
    type Event = i_slint_core::platform::WindowEvent;
    type Metadata = ();
    type Ret = ();
    type Error = std::io::Error;

    fn process_events<F>(
        &mut self,
        _readiness: calloop::Readiness,
        token: calloop::Token,
        _callback: F,
    ) -> Result<calloop::PostAction, Self::Error>
    where
        F: FnMut(Self::Event, &mut Self::Metadata) -> Self::Ret,
    {
        if Some(token) != self.token {
            return Ok(calloop::PostAction::Continue);
        }

        self.libinput.dispatch()?;

        let Some(adapter) = self.window.borrow().clone() else {
            return Ok(calloop::PostAction::Continue);
        };
        let window = adapter.window();
        let screen_size = window.size().to_logical(window.scale_factor());

        for event in &mut self.libinput {
            if self.libinput_event_hook.as_ref().is_some_and(|hook| hook(&event)) {
                continue;
            };
            match event {
                input::Event::Pointer(pointer_event) => {
                    match pointer_event {
                        input::event::PointerEvent::Motion(motion_event) => {
                            let mut mouse_pos =
                                self.mouse_pos.as_ref().get().unwrap_or(LogicalPosition {
                                    x: screen_size.width / 2.,
                                    y: screen_size.height / 2.,
                                });
                            mouse_pos.x = (mouse_pos.x + motion_event.dx() as f32)
                                .clamp(0., screen_size.width);
                            mouse_pos.y = (mouse_pos.y + motion_event.dy() as f32)
                                .clamp(0., screen_size.height);
                            self.mouse_pos.set(Some(mouse_pos));
                            let event = WindowEvent::PointerMoved { position: mouse_pos };
                            window.try_dispatch_event(event).map_err(Self::Error::other)?;
                        }
                        input::event::PointerEvent::MotionAbsolute(abs_motion_event) => {
                            let mouse_pos = LogicalPosition {
                                x: abs_motion_event.absolute_x_transformed(screen_size.width as u32)
                                    as _,
                                y: abs_motion_event
                                    .absolute_y_transformed(screen_size.height as u32)
                                    as _,
                            };
                            self.mouse_pos.set(Some(mouse_pos));
                            let event = WindowEvent::PointerMoved { position: mouse_pos };
                            window.try_dispatch_event(event).map_err(Self::Error::other)?;
                        }
                        input::event::PointerEvent::Button(button_event) => {
                            // https://github.com/torvalds/linux/blob/0dd2a6fb1e34d6dcb96806bc6b111388ad324722/include/uapi/linux/input-event-codes.h#L355
                            let button = match button_event.button() {
                                0x110 => PointerEventButton::Left,
                                0x111 => PointerEventButton::Right,
                                0x112 => PointerEventButton::Middle,
                                0x116 => PointerEventButton::Back,
                                0x115 => PointerEventButton::Forward,
                                _ => PointerEventButton::Other,
                            };
                            let mouse_pos = self.mouse_pos.as_ref().get().unwrap_or_default();
                            let event = match button_event.button_state() {
                                input::event::tablet_pad::ButtonState::Pressed => {
                                    WindowEvent::PointerPressed { position: mouse_pos, button }
                                }
                                input::event::tablet_pad::ButtonState::Released => {
                                    WindowEvent::PointerReleased { position: mouse_pos, button }
                                }
                            };
                            window.try_dispatch_event(event).map_err(Self::Error::other)?;
                        }
                        _ => {}
                    }
                }
                input::Event::Touch(touch_event) => match touch_event {
                    input::event::TouchEvent::Down(touch_down_event) => {
                        let pos = LogicalPosition::new(
                            touch_down_event.x_transformed(screen_size.width as u32) as _,
                            touch_down_event.y_transformed(screen_size.height as u32) as _,
                        );
                        let slot = touch_down_event.slot().unwrap_or(0) as i32;
                        set_touch_pos(&mut self.last_touch_positions, slot, pos);
                        WindowInner::from_pub(window).process_touch_input(
                            slot,
                            logical_point_from_api(pos),
                            i_slint_core::input::TouchPhase::Started,
                        );
                    }
                    input::event::TouchEvent::Up(touch_up_event) => {
                        let slot = touch_up_event.slot().unwrap_or(0) as i32;
                        let pos = take_touch_pos(&mut self.last_touch_positions, slot);
                        WindowInner::from_pub(window).process_touch_input(
                            slot,
                            logical_point_from_api(pos),
                            i_slint_core::input::TouchPhase::Ended,
                        );
                    }
                    input::event::TouchEvent::Motion(touch_motion_event) => {
                        let pos = LogicalPosition::new(
                            touch_motion_event.x_transformed(screen_size.width as u32) as _,
                            touch_motion_event.y_transformed(screen_size.height as u32) as _,
                        );
                        let slot = touch_motion_event.slot().unwrap_or(0) as i32;
                        set_touch_pos(&mut self.last_touch_positions, slot, pos);
                        WindowInner::from_pub(window).process_touch_input(
                            slot,
                            logical_point_from_api(pos),
                            i_slint_core::input::TouchPhase::Moved,
                        );
                    }
                    input::event::TouchEvent::Cancel(touch_cancel_event) => {
                        let slot = touch_cancel_event.slot().unwrap_or(0) as i32;
                        let pos = take_touch_pos(&mut self.last_touch_positions, slot);
                        WindowInner::from_pub(window).process_touch_input(
                            slot,
                            logical_point_from_api(pos),
                            i_slint_core::input::TouchPhase::Cancelled,
                        );
                    }
                    _ => {}
                },
                input::Event::Keyboard(input::event::KeyboardEvent::Key(key_event)) => {
                    // On Linux key codes have a fixed offset of 8: https://docs.rs/xkbcommon/0.6.0/xkbcommon/xkb/struct.Keycode.html
                    let key_code = xkb::Keycode::new(key_event.key() + 8);
                    let state = key_event.key_state();

                    let xkb_key_state = self.keystate.get_or_insert_with(|| {
                        let xkb_context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
                        let keymap =
                            xkb::Keymap::new_from_names(&xkb_context, "", "", "", "", None, 0)
                                .expect("Error compiling keymap");
                        xkb::State::new(&keymap)
                    });

                    let sym = xkb_key_state.key_get_one_sym(key_code);

                    xkb_key_state.update_key(
                        key_code,
                        match state {
                            input::event::tablet_pad::KeyState::Pressed => xkb::KeyDirection::Down,
                            input::event::tablet_pad::KeyState::Released => xkb::KeyDirection::Up,
                        },
                    );

                    let control = xkb_key_state
                        .mod_name_is_active(xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE);
                    let alt = xkb_key_state
                        .mod_name_is_active(xkb::MOD_NAME_ALT, xkb::STATE_MODS_EFFECTIVE);

                    if state == KeyState::Pressed {
                        //eprintln!(
                        //"key {} state {:#?} sym {:x} control {control} alt {alt}",
                        //key_code, state, sym
                        //);

                        if (sym == xkb::Keysym::Delete || sym == xkb::Keysym::BackSpace)
                            && alt
                            && control
                        {
                            i_slint_core::api::quit_event_loop()
                                .expect("Unable to quit event loop multiple times");
                        } else if (xkb::Keysym::XF86_Switch_VT_1..=xkb::Keysym::XF86_Switch_VT_12)
                            .contains(&sym)
                        {
                            // let target_vt = (sym - xkb::KEY_XF86Switch_VT_1 + 1) as i32;
                            // TODO: eprintln!("switch vt {target_vt}");
                        }
                    }

                    if let Some(text) = map_key_sym(sym) {
                        let event = match state {
                            KeyState::Pressed => WindowEvent::KeyPressed { text },
                            KeyState::Released => WindowEvent::KeyReleased { text },
                        };
                        window.try_dispatch_event(event).map_err(Self::Error::other)?;
                    }
                }
                _ => {}
            }
            //println!("Got event: {:?}", event);
        }

        Ok(calloop::PostAction::Continue)
    }

    fn register(
        &mut self,
        poll: &mut calloop::Poll,
        token_factory: &mut calloop::TokenFactory,
    ) -> calloop::Result<()> {
        self.token = Some(token_factory.token());
        unsafe {
            poll.register(
                &self.libinput,
                calloop::Interest::READ,
                calloop::Mode::Level,
                self.token.unwrap(),
            )
        }
    }

    fn reregister(
        &mut self,
        poll: &mut calloop::Poll,
        token_factory: &mut calloop::TokenFactory,
    ) -> calloop::Result<()> {
        self.token = Some(token_factory.token());
        poll.reregister(
            &self.libinput,
            calloop::Interest::READ,
            calloop::Mode::Level,
            self.token.unwrap(),
        )
    }

    fn unregister(&mut self, poll: &mut calloop::Poll) -> calloop::Result<()> {
        self.token = None;
        poll.unregister(&self.libinput)
    }
}

fn map_key_sym(sym: xkb::Keysym) -> Option<SharedString> {
    macro_rules! keysym_to_string {
        ($($char:literal # $name:ident # $($shifted:ident)? $(=> $($_muda:ident)? # $($_qt:ident)|* # $($_winit:ident $(($_pos:ident))?)|* # $($xkb:ident)|* )? ;)*) => {
            match(sym) {
                $($($(xkb::Keysym::$xkb => $char,)*)?)*
                _ => std::char::from_u32(xkbcommon::xkb::keysym_to_utf32(sym))?,
            }
        };
    }
    let char = i_slint_common::for_each_keys!(keysym_to_string);
    Some(char.into())
}
