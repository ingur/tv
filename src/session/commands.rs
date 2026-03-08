//! Command implementations for session requests.

use crate::types::{ExecResponse, PeekResponse, Response, SessionStateResponse, TextContent};

impl super::Session {
    fn session_id(&self) -> String {
        self.session_id.clone().unwrap_or_default()
    }

    pub fn cmd_show(&mut self) -> Response {
        self.visible = true;
        self.view.mark_dirty();
        Response::ok(SessionStateResponse {
            id: self.session_id(),
            tag: self.tag.clone(),
            visible: Some(true),
        })
    }

    pub fn cmd_hide(&mut self) -> Response {
        self.visible = false;
        self.view.mark_dirty();
        Response::ok(SessionStateResponse {
            id: self.session_id(),
            tag: self.tag.clone(),
            visible: Some(false),
        })
    }

    pub fn cmd_tag(&mut self, new_tag: Option<String>, delete: bool) -> Response {
        if delete {
            self.tag = None;
        } else if let Some(tag) = new_tag {
            self.tag = Some(tag);
        }
        self.view.mark_dirty();
        Response::ok(SessionStateResponse {
            id: self.session_id(),
            tag: self.tag.clone(),
            visible: None,
        })
    }

    pub fn cmd_clear(&mut self) -> Response {
        self.text_parser.clear();
        Response::ok(SessionStateResponse {
            id: self.session_id(),
            tag: None,
            visible: None,
        })
    }

    pub fn cmd_peek(&self) -> Response {
        let content = self.text_parser.render_screen();
        let (cursor_row, cursor_col) = self.text_parser.cursor();
        let (rows, cols) = self.text_parser.dimensions();
        let in_alt_screen = self.text_parser.in_alt_screen();

        Response::ok(PeekResponse {
            content,
            cursor_row,
            cursor_col,
            rows,
            cols,
            in_alt_screen,
        })
    }

    pub fn cmd_history(&self, count: Option<usize>, offset: Option<usize>) -> Response {
        let content = self.text_parser.get_history(count, offset);
        Response::ok(TextContent { content })
    }

    pub fn build_exec_response(&self) -> Response {
        Response::ok(ExecResponse {
            id: self.session_id(),
        })
    }
}
