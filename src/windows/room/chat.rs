use std::borrow::Cow;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use matrix_sdk::{
    attachment::AttachmentConfig,
    media::{MediaFormat, MediaRequest},
    room::Room as MatrixRoom,
    ruma::{
        events::room::message::{MessageType, RoomMessageEventContent, TextMessageEventContent},
        OwnedRoomId,
        RoomId,
    },
};

use modalkit::{
    tui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget},
    widgets::textbox::{TextBox, TextBoxState},
    widgets::TerminalCursor,
    widgets::{PromptActions, WindowOps},
};

use modalkit::editing::{
    action::{
        EditError,
        EditInfo,
        EditResult,
        Editable,
        EditorAction,
        InfoMessage,
        Jumpable,
        PromptAction,
        Promptable,
        Scrollable,
        UIError,
    },
    base::{CloseFlags, Count, MoveDir1D, PositionList, ScrollStyle, WordStyle},
    context::Resolve,
    history::{self, HistoryList},
    rope::EditRope,
};

use crate::base::{
    IambAction,
    IambBufferId,
    IambError,
    IambInfo,
    IambResult,
    MessageAction,
    ProgramAction,
    ProgramContext,
    ProgramStore,
    RoomFocus,
    SendAction,
};

use crate::message::{Message, MessageContent, MessageTimeStamp};

use super::scrollback::{Scrollback, ScrollbackState};

pub struct ChatState {
    room_id: OwnedRoomId,
    room: MatrixRoom,

    tbox: TextBoxState<IambInfo>,
    sent: HistoryList<EditRope>,
    sent_scrollback: history::ScrollbackState,

    scrollback: ScrollbackState,
    focus: RoomFocus,
}

impl ChatState {
    pub fn new(room: MatrixRoom, store: &mut ProgramStore) -> Self {
        let room_id = room.room_id().to_owned();
        let scrollback = ScrollbackState::new(room_id.clone());
        let id = IambBufferId::Room(room_id.clone(), RoomFocus::MessageBar);
        let ebuf = store.load_buffer(id);
        let tbox = TextBoxState::new(ebuf);

        ChatState {
            room_id,
            room,

            tbox,
            sent: HistoryList::new(EditRope::from(""), 100),
            sent_scrollback: history::ScrollbackState::Pending,

            scrollback,
            focus: RoomFocus::MessageBar,
        }
    }

    pub async fn message_command(
        &mut self,
        act: MessageAction,
        _: ProgramContext,
        store: &mut ProgramStore,
    ) -> IambResult<EditInfo> {
        let client = &store.application.worker.client;

        let settings = &store.application.settings;
        let info = store.application.rooms.entry(self.room_id.clone()).or_default();

        let msg = self.scrollback.get_mut(info).ok_or(IambError::NoSelectedMessage)?;

        match act {
            MessageAction::Download(filename, force) => {
                if let MessageContent::Original(ev) = &msg.content {
                    let media = client.media();

                    let mut filename = match filename {
                        Some(f) => PathBuf::from(f),
                        None => settings.dirs.downloads.clone(),
                    };

                    let source = match &ev.msgtype {
                        MessageType::Audio(c) => {
                            if filename.is_dir() {
                                filename.push(c.body.as_str());
                            }

                            c.source.clone()
                        },
                        MessageType::File(c) => {
                            if filename.is_dir() {
                                if let Some(name) = &c.filename {
                                    filename.push(name);
                                } else {
                                    filename.push(c.body.as_str());
                                }
                            }

                            c.source.clone()
                        },
                        MessageType::Image(c) => {
                            if filename.is_dir() {
                                filename.push(c.body.as_str());
                            }

                            c.source.clone()
                        },
                        MessageType::Video(c) => {
                            if filename.is_dir() {
                                filename.push(c.body.as_str());
                            }

                            c.source.clone()
                        },
                        _ => {
                            return Err(IambError::NoAttachment.into());
                        },
                    };

                    if !force && filename.exists() {
                        let msg = format!(
                            "The file {} already exists; use :download! to overwrite it.",
                            filename.display()
                        );
                        let err = UIError::Failure(msg);

                        return Err(err);
                    }

                    let req = MediaRequest { source, format: MediaFormat::File };

                    let bytes =
                        media.get_media_content(&req, true).await.map_err(IambError::from)?;

                    fs::write(filename.as_path(), bytes.as_slice())?;

                    msg.downloaded = true;

                    let info = InfoMessage::from(format!(
                        "Attachment downloaded to {}",
                        filename.display()
                    ));

                    return Ok(info.into());
                }

                Err(IambError::NoAttachment.into())
            },
        }
    }

    pub async fn send_command(
        &mut self,
        act: SendAction,
        _: ProgramContext,
        store: &mut ProgramStore,
    ) -> IambResult<EditInfo> {
        let room = store
            .application
            .worker
            .client
            .get_joined_room(self.id())
            .ok_or(IambError::NotJoined)?;

        let (event_id, msg) = match act {
            SendAction::Submit => {
                let msg = self.tbox.get_text();

                if msg.is_empty() {
                    return Ok(None);
                }

                let msg = TextMessageEventContent::plain(msg);
                let msg = MessageType::Text(msg);
                let msg = RoomMessageEventContent::new(msg);

                // XXX: second parameter can be a locally unique transaction id.
                // Useful for doing retries.
                let resp = room.send(msg.clone(), None).await.map_err(IambError::from)?;
                let event_id = resp.event_id;

                // Clear the TextBoxState contents now that the message is sent.
                self.tbox.reset();

                (event_id, msg)
            },
            SendAction::Upload(file) => {
                let path = Path::new(file.as_str());
                let mime = mime_guess::from_path(path).first_or(mime::APPLICATION_OCTET_STREAM);

                let bytes = fs::read(path)?;
                let name = path
                    .file_name()
                    .map(OsStr::to_string_lossy)
                    .unwrap_or_else(|| Cow::from("Attachment"));
                let config = AttachmentConfig::new();

                let resp = room
                    .send_attachment(name.as_ref(), &mime, bytes.as_ref(), config)
                    .await
                    .map_err(IambError::from)?;

                // Mock up the local echo message for the scrollback.
                let msg = TextMessageEventContent::plain(format!("[Attached File: {}]", name));
                let msg = MessageType::Text(msg);
                let msg = RoomMessageEventContent::new(msg);

                (resp.event_id, msg)
            },
        };

        let user = store.application.settings.profile.user_id.clone();
        let info = store.application.get_room_info(self.id().to_owned());
        let key = (MessageTimeStamp::LocalEcho, event_id);
        let msg = MessageContent::Original(msg.into());
        let msg = Message::new(msg, user, MessageTimeStamp::LocalEcho);
        info.messages.insert(key, msg);

        Ok(None)
    }

    pub fn focus_toggle(&mut self) {
        self.focus = match self.focus {
            RoomFocus::Scrollback => RoomFocus::MessageBar,
            RoomFocus::MessageBar => RoomFocus::Scrollback,
        };
    }

    pub fn room(&self) -> &MatrixRoom {
        &self.room
    }

    pub fn id(&self) -> &RoomId {
        &self.room_id
    }

    pub fn typing_notice(
        &self,
        act: &EditorAction,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) {
        if !self.focus.is_msgbar() || act.is_readonly(ctx) {
            return;
        }

        if !store.application.settings.tunables.typing_notice {
            return;
        }

        store.application.worker.typing_notice(self.room_id.clone());
    }
}

macro_rules! delegate {
    ($s: expr, $id: ident => $e: expr) => {
        match $s.focus {
            RoomFocus::Scrollback => {
                match $s {
                    ChatState { scrollback: $id, .. } => $e,
                }
            },
            RoomFocus::MessageBar => {
                match $s {
                    ChatState { tbox: $id, .. } => $e,
                }
            },
        }
    };
}

impl WindowOps<IambInfo> for ChatState {
    fn draw(&mut self, area: Rect, buf: &mut Buffer, focused: bool, store: &mut ProgramStore) {
        Chat::new(store).focus(focused).render(area, buf, self)
    }

    fn dup(&self, store: &mut ProgramStore) -> Self {
        // XXX: I want each WindowSlot to have its own shared buffer, instead of each Room; need to
        // find a good way to pass that info here so that it can be part of the content id.
        let id = IambBufferId::Room(self.room_id.clone(), RoomFocus::MessageBar);
        let ebuf = store.load_buffer(id);
        let tbox = TextBoxState::new(ebuf);

        ChatState {
            room_id: self.room_id.clone(),
            room: self.room.clone(),

            tbox,
            sent: self.sent.clone(),
            sent_scrollback: history::ScrollbackState::Pending,

            scrollback: self.scrollback.dup(store),
            focus: self.focus,
        }
    }

    fn close(&mut self, _: CloseFlags, _: &mut ProgramStore) -> bool {
        // XXX: what's the right closing behaviour for a room?
        // Should write send a message?
        true
    }

    fn get_cursor_word(&self, style: &WordStyle) -> Option<String> {
        delegate!(self, w => w.get_cursor_word(style))
    }

    fn get_selected_word(&self) -> Option<String> {
        delegate!(self, w => w.get_selected_word())
    }
}

impl Editable<ProgramContext, ProgramStore, IambInfo> for ChatState {
    fn editor_command(
        &mut self,
        act: &EditorAction,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        self.typing_notice(act, ctx, store);

        match delegate!(self, w => w.editor_command(act, ctx, store)) {
            res @ Ok(_) => res,
            Err(EditError::WrongBuffer(IambBufferId::Room(room_id, focus)))
                if room_id == self.room_id && act.is_switchable(ctx) =>
            {
                // Switch focus.
                self.focus = focus;

                // Run command again.
                delegate!(self, w => w.editor_command(act, ctx, store))
            },
            res @ Err(_) => res,
        }
    }
}

impl TerminalCursor for ChatState {
    fn get_term_cursor(&self) -> Option<(u16, u16)> {
        delegate!(self, w => w.get_term_cursor())
    }
}

impl Jumpable<ProgramContext, IambInfo> for ChatState {
    fn jump(
        &mut self,
        list: PositionList,
        dir: MoveDir1D,
        count: usize,
        ctx: &ProgramContext,
    ) -> IambResult<usize> {
        delegate!(self, w => w.jump(list, dir, count, ctx))
    }
}

impl Scrollable<ProgramContext, ProgramStore, IambInfo> for ChatState {
    fn scroll(
        &mut self,
        style: &ScrollStyle,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        // Send all scroll commands to the scrollback.
        //
        // If there's enough message text for scrolling to be necessary,
        // navigating with movement keys should be enough to do the job.
        self.scrollback.scroll(style, ctx, store)
    }
}

impl PromptActions<ProgramContext, ProgramStore, IambInfo> for ChatState {
    fn submit(
        &mut self,
        ctx: &ProgramContext,
        _: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        let act = SendAction::Submit;

        Ok(vec![(IambAction::from(act).into(), ctx.clone())])
    }

    fn abort(
        &mut self,
        empty: bool,
        _: &ProgramContext,
        _: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        let text = self.tbox.get();

        if empty && text.is_blank() {
            return Ok(vec![]);
        }

        let text = self.tbox.reset().trim();

        if text.is_empty() {
            let _ = self.sent.end();
        } else {
            self.sent.select(text);
        }

        return Ok(vec![]);
    }

    fn recall(
        &mut self,
        dir: &MoveDir1D,
        count: &Count,
        ctx: &ProgramContext,
        _: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        let count = ctx.resolve(count);
        let rope = self.tbox.get();

        let text = self.sent.recall(&rope, &mut self.sent_scrollback, *dir, count);

        if let Some(text) = text {
            self.tbox.set_text(text);
        }

        Ok(vec![])
    }
}

impl Promptable<ProgramContext, ProgramStore, IambInfo> for ChatState {
    fn prompt(
        &mut self,
        act: &PromptAction,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        if let RoomFocus::Scrollback = self.focus {
            return Ok(vec![]);
        }

        match act {
            PromptAction::Submit => self.submit(ctx, store),
            PromptAction::Abort(empty) => self.abort(*empty, ctx, store),
            PromptAction::Recall(dir, count) => self.recall(dir, count, ctx, store),
            _ => Err(EditError::Unimplemented("unknown prompt action".to_string())),
        }
    }
}

pub struct Chat<'a> {
    store: &'a mut ProgramStore,
    focused: bool,
}

impl<'a> Chat<'a> {
    pub fn new(store: &'a mut ProgramStore) -> Chat<'a> {
        Chat { store, focused: false }
    }

    pub fn focus(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }
}

impl<'a> StatefulWidget for Chat<'a> {
    type State = ChatState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let lines = state.tbox.has_lines(5).max(1) as u16;
        let drawh = area.height;
        let texth = lines.min(drawh).clamp(1, 5);
        let scrollh = drawh.saturating_sub(texth);

        let scrollarea = Rect::new(area.x, area.y, area.width, scrollh);
        let textarea = Rect::new(scrollarea.x, scrollarea.y + scrollh, scrollarea.width, texth);

        let scrollback_focused = state.focus.is_scrollback() && self.focused;
        let scrollback = Scrollback::new(self.store).focus(scrollback_focused);
        scrollback.render(scrollarea, buf, &mut state.scrollback);

        let prompt = if self.focused { "> " } else { "  " };

        let tbox = TextBox::new().prompt(prompt);
        tbox.render(textarea, buf, &mut state.tbox);
    }
}
