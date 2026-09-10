//! Ctrl+V recognition on the pool's client-to-PTY stream. Only the paste key
//! asks the caller for replacement input; ordinary bytes and bracketed-paste
//! payloads pass through. The caller owns agent selection and image retrieval.
//! Escape prefixes are bounded and can be flushed after a short input timeout,
//! so a standalone Escape never waits for the user's next key.

#[derive(Debug, Default)]
pub struct PasteInput {
    prefix: Vec<u8>,
    bracketed: bool,
    oversized_csi: bool,
    string: Option<bool>,
    string_escape: bool,
}

impl PasteInput {
    pub fn process(&mut self, bytes: &[u8], mut paste: impl FnMut() -> Option<Vec<u8>>) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            if let Some(osc) = self.string {
                out.push(byte);
                if (osc && byte == 7)
                    || (self.string_escape && byte == b'\\')
                    || matches!(byte, 0x18 | 0x1a)
                {
                    self.string = None;
                }
                self.string_escape = byte == 0x1b;
            } else if byte == 0x1b {
                out.append(&mut self.prefix);
                self.oversized_csi = false;
                self.prefix.push(byte);
            } else if self.oversized_csi {
                out.push(byte);
                if (0x40..=0x7e).contains(&byte) {
                    self.oversized_csi = false;
                }
            } else if self.prefix.is_empty() {
                if byte == 0x16 && !self.bracketed {
                    out.extend(paste().unwrap_or_else(|| vec![byte]));
                } else {
                    out.push(byte);
                }
            } else {
                self.prefix.push(byte);
                if self.prefix.len() == 2 {
                    if byte != b'[' {
                        if matches!(byte, b']' | b'P' | b'X' | b'^' | b'_') {
                            self.string = Some(byte == b']');
                            self.string_escape = false;
                        }
                        out.append(&mut self.prefix);
                    }
                } else if (0x40..=0x7e).contains(&byte) {
                    if self.prefix == b"\x1b[200~" {
                        self.bracketed = true;
                    } else if self.prefix == b"\x1b[201~" {
                        self.bracketed = false;
                    } else if !self.bracketed
                        && is_paste_key(&self.prefix)
                        && let Some(replacement) = paste()
                    {
                        out.extend(replacement);
                        self.prefix.clear();
                    }
                    out.append(&mut self.prefix);
                } else if self.prefix.len() >= 64 || !(0x20..=0x3f).contains(&byte) {
                    self.oversized_csi = true;
                    out.append(&mut self.prefix);
                }
            }
        }
        out
    }

    pub fn pending(&self) -> bool {
        !self.prefix.is_empty()
    }

    pub fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.prefix)
    }
}

fn is_paste_key(sequence: &[u8]) -> bool {
    let Ok(body) = std::str::from_utf8(&sequence[2..]) else {
        return false;
    };
    if let Some(body) = body.strip_suffix('u') {
        let Some((key, modifiers)) = body.split_once(';') else {
            return false;
        };
        let key: Vec<_> = key.split(':').collect();
        if key[0] != "118"
            || key.len() > 3
            || key[1..]
                .iter()
                .any(|k| !k.is_empty() && k.parse::<u32>().is_err())
        {
            return false;
        }
        let mut modifiers = modifiers.split(':');
        let Some(mods) = modifiers.next().and_then(|m| m.parse::<u16>().ok()) else {
            return false;
        };
        // Ctrl alone; lock modifiers do not change the key. Ignore releases.
        mods.checked_sub(1).is_some_and(|m| m & !192 == 4)
            && matches!(modifiers.next(), None | Some("1" | "2"))
            && modifiers.next().is_none()
    } else {
        body == "27;5;118~"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_keys_survive_every_split_and_keep_surrounding_typing() {
        for key in [
            b"\x16".as_slice(),
            b"\x1b[118;5u",
            b"\x1b[118:86;5u",
            b"\x1b[118::118;5u",
            b"\x1b[118:86:118;5:1u",
            b"\x1b[118;5:2u",
            b"\x1b[118;69u", // Caps Lock does not change Ctrl+V.
            b"\x1b[27;5;118~",
        ] {
            let bytes = [b"draft ", key, b" after"].concat();
            for split in 0..=bytes.len() {
                let mut input = PasteInput::default();
                let mut calls = 0;
                let mut paste = || {
                    calls += 1;
                    Some(b"<image>".to_vec())
                };
                let mut out = input.process(&bytes[..split], &mut paste);
                out.extend(input.process(&bytes[split..], &mut paste));
                out.extend(input.flush());
                assert_eq!(out, b"draft <image> after", "key {key:?}, split {split}");
                assert_eq!(calls, 1);
            }
        }
    }

    #[test]
    fn pasted_text_and_other_keys_never_fetch_an_image() {
        let bytes = concat!(
            "\x1b[200~literal \u{16} \x1b[118;5u pasted\x1b[201~",
            "\x1b[A\x1b[118;1u\x1b[118;6u\x1b[118;5:3u\x1b[27;5;117~",
            "\x1bxordinary text",
            "\x1b]clipboard response \u{16}\x1b[118;5u\u{7}",
            "\x1bPstring \u{16}\x1b[118;5u\x1b\\",
        )
        .as_bytes();
        let mut input = PasteInput::default();
        let mut out = Vec::new();
        for byte in bytes {
            out.extend(input.process(&[*byte], || panic!("must not fetch clipboard")));
        }
        out.extend(input.flush());
        assert_eq!(out, bytes);
    }

    #[test]
    fn failed_paste_keeps_the_original_key_and_escape_can_flush() {
        let mut input = PasteInput::default();
        let key = b"\x1b[118;5u";
        assert_eq!(input.process(key, || None), key);
        assert!(input.process(b"\x1b", || None).is_empty());
        assert!(input.pending());
        assert_eq!(input.flush(), b"\x1b");
        assert!(!input.pending());
        assert_eq!(
            input.process(b"[118;5u", || panic!("already flushed")),
            b"[118;5u"
        );
    }
}
