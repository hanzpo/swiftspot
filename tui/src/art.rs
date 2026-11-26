use std::io::Write;

use base64::Engine as _;
use ratatui::layout::Rect;

const IMAGE: u32 = 7177;
const PIXELS: u32 = 256;

pub fn supported() -> bool {
    if std::env::var_os("SWIFTSPOT_ART").is_some_and(|value| value == "0") {
        return false;
    }
    let program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let term = std::env::var("TERM").unwrap_or_default();
    matches!(program.as_str(), "rex" | "ghostty" | "WezTerm" | "kitty")
        || term.contains("kitty")
        || term.contains("ghostty")
}

struct Picture {
    url: String,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

pub struct Art {
    enabled: bool,
    picture: Option<Picture>,
    sent: Option<String>,
    shown: Option<(String, Rect)>,
}

impl Art {
    pub fn new() -> Self {
        Self {
            enabled: supported(),
            picture: None,
            sent: None,
            shown: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn has(&self, url: &str) -> bool {
        self.picture
            .as_ref()
            .is_some_and(|picture| picture.url == url)
    }

    pub fn set(&mut self, url: String, bytes: &[u8]) {
        let Ok(decoded) = image::load_from_memory(bytes) else {
            return;
        };
        let rgba = decoded.thumbnail(PIXELS, PIXELS).to_rgba8();
        self.picture = Some(Picture {
            url,
            width: rgba.width(),
            height: rgba.height(),
            rgba: rgba.into_raw(),
        });
    }

    pub fn invalidate(&mut self) {
        self.shown = None;
    }

    pub fn sync(&mut self, out: &mut impl Write, url: Option<&str>, area: Option<Rect>) {
        if !self.enabled {
            return;
        }
        let want = match (url, area, &self.picture) {
            (Some(url), Some(area), Some(picture))
                if picture.url == url && area.width > 0 && area.height > 0 =>
            {
                Some((url.to_string(), area))
            }
            _ => None,
        };
        if want == self.shown {
            return;
        }
        let mut bytes = Vec::new();
        match (&want, &self.picture) {
            (Some((url, area)), Some(picture)) => {
                if self.sent.as_deref() != Some(url.as_str()) {
                    transmit(&mut bytes, picture);
                    self.sent = Some(url.clone());
                }
                place(&mut bytes, *area);
            }
            _ => bytes.extend_from_slice(format!("\x1b_Ga=d,d=i,i={IMAGE},q=2\x1b\\").as_bytes()),
        }
        if out.write_all(&bytes).and_then(|()| out.flush()).is_ok() {
            self.shown = want;
        }
    }

    pub fn forget(&self, out: &mut impl Write) {
        if self.enabled {
            let _ = write!(out, "\x1b_Ga=d,d=I,i={IMAGE},q=2\x1b\\");
            let _ = out.flush();
        }
    }
}

fn transmit(out: &mut Vec<u8>, picture: &Picture) {
    let data = base64::engine::general_purpose::STANDARD.encode(&picture.rgba);
    let chunks: Vec<&[u8]> = data.as_bytes().chunks(4096).collect();
    for (index, chunk) in chunks.iter().enumerate() {
        let more = u8::from(index + 1 < chunks.len());
        if index == 0 {
            out.extend_from_slice(
                format!(
                    "\x1b_Ga=t,f=32,s={},v={},i={IMAGE},q=2,m={more};",
                    picture.width, picture.height
                )
                .as_bytes(),
            );
        } else {
            out.extend_from_slice(format!("\x1b_Gm={more};").as_bytes());
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
}

fn place(out: &mut Vec<u8>, area: Rect) {
    out.extend_from_slice(
        format!(
            "\x1b7\x1b[{};{}H\x1b_Ga=p,i={IMAGE},p=1,c={},r={},C=1,q=2\x1b\\\x1b8",
            area.y + 1,
            area.x + 1,
            area.width,
            area.height
        )
        .as_bytes(),
    );
}
