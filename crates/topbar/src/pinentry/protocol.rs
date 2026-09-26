//! The byte-oriented subset of the Assuan pinentry protocol used by GnuPG.
//!
//! UI code never parses a protocol line.  This module owns the session state
//! and exposes typed prompt requests, which keeps passphrases on the protocol
//! pipes and makes transcript fixtures possible without GTK.

use std::io::{self, BufRead, Write};

/// Largest accepted Assuan request, including its newline.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

const GREETING: &[u8] = b"OK Pleased to meet you";
const OK: &[u8] = b"OK";
const OK_CLOSING: &[u8] = b"OK closing connection";
const ERR_CANCELLED: &[u8] = b"ERR 83886179 Operation cancelled <Pinentry>";
const ERR_TIMEOUT: &[u8] = b"ERR 83886142 Timeout <Pinentry>";
const ERR_NOT_CONFIRMED: &[u8] = b"ERR 83886194 Not confirmed <Pinentry>";
const ERR_PINENTRY: &[u8] = b"ERR 83886166 Pinentry error <Pinentry>";
const ERR_PARAMETER: &[u8] = b"ERR 83886360 IPC parameter error <Pinentry>";
const ERR_UNKNOWN: &[u8] = b"ERR 83886255 Unknown command <Pinentry>";

const DEFAULT_PROMPT: &[u8] = b"Passphrase: ";
const DEFAULT_REPEAT_PROMPT: &[u8] = b"Repeat: ";
const DEFAULT_REPEAT_ERROR: &[u8] = b"Passphrases do not match.";
const DEFAULT_OK: &[u8] = b"OK";
const DEFAULT_CANCEL: &[u8] = b"Cancel";
/// Upper bound that keeps deadline arithmetic representable and prevents a
/// malformed peer from reserving the input lock indefinitely.
pub const MAX_TIMEOUT_SECONDS: u64 = 24 * 60 * 60;

/// A request the graphical pinentry needs to render.
#[derive(Clone)]
pub struct Prompt {
    /// Prompt kind.
    pub kind: PromptKind,
    /// Plain protocol context, never interpreted as markup.
    pub context: PromptContext,
    /// Seconds covering lock acquisition and visible interaction, if set.
    pub timeout_seconds: u64,
}

/// The visual interaction requested by GnuPG.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// Ask for a passphrase.
    Password,
    /// Ask for a second passphrase.
    Repeat,
    /// Ask for an explicit decision.
    Confirmation { one_button: bool },
    /// Present information with an acknowledgement button.
    Message,
}

/// Plain text and labels accumulated by pinentry context setters.
#[derive(Clone, Default)]
pub struct PromptContext {
    /// Optional window heading.
    pub title: Vec<u8>,
    /// Key or application context.
    pub user_data: Vec<u8>,
    /// Description supplied by GnuPG.
    pub description: Vec<u8>,
    /// Error shown above the primary prompt.
    pub error: Vec<u8>,
    /// The password field label.
    pub prompt: Vec<u8>,
    /// Positive action label.
    pub ok: Vec<u8>,
    /// Negative action label.
    pub cancel: Vec<u8>,
}

/// Result supplied by the graphical process boundary.
pub enum PromptResult {
    /// A password, owned by this response and wiped after it is encoded.
    Password(Secret),
    /// The user accepted a non-password request.
    Accepted,
    /// The user escaped or cancelled the prompt.
    Cancelled,
    /// The user explicitly declined a confirmation.
    Denied,
    /// The lock wait or visible prompt reached its deadline.
    TimedOut,
    /// GTK could not show the request.
    Failed,
}

/// A passphrase buffer that wipes its allocation on drop.
pub struct Secret(Vec<u8>);

impl Secret {
    /// Take ownership of password bytes copied from GTK's password buffer.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

#[derive(Default)]
struct State {
    description: Vec<u8>,
    user_data: Vec<u8>,
    prompt: Vec<u8>,
    title: Vec<u8>,
    error: Vec<u8>,
    ok: Vec<u8>,
    cancel: Vec<u8>,
    timeout_seconds: u64,
    display: Vec<u8>,
    ttyname: Vec<u8>,
    ttytype: Vec<u8>,
    repeat_prompt: Option<Vec<u8>>,
    repeat_error: Option<Vec<u8>>,
}

impl State {
    fn reset(&mut self) {
        wipe(&mut self.description);
        wipe(&mut self.user_data);
        wipe(&mut self.prompt);
        wipe(&mut self.title);
        wipe(&mut self.error);
        wipe(&mut self.ok);
        wipe(&mut self.cancel);
        wipe(&mut self.display);
        wipe(&mut self.ttyname);
        wipe(&mut self.ttytype);
        if let Some(mut value) = self.repeat_prompt.take() {
            wipe(&mut value);
        }
        if let Some(mut value) = self.repeat_error.take() {
            wipe(&mut value);
        }
        self.prompt.extend_from_slice(DEFAULT_PROMPT);
        self.ok.extend_from_slice(DEFAULT_OK);
        self.cancel.extend_from_slice(DEFAULT_CANCEL);
        self.timeout_seconds = 0;
    }

    fn context(&self, prompt: &[u8], error: &[u8]) -> PromptContext {
        PromptContext {
            title: self.title.clone(),
            user_data: self.user_data.clone(),
            description: self.description.clone(),
            error: error.to_vec(),
            prompt: prompt.to_vec(),
            ok: self.ok.clone(),
            cancel: self.cancel.clone(),
        }
    }
}

/// A synchronous protocol server. The prompt callback may block while GTK
/// interacts with the user, but protocol parsing itself remains byte based.
pub struct Worker {
    state: State,
}

impl Default for Worker {
    fn default() -> Self {
        let mut state = State::default();
        state.reset();
        Self { state }
    }
}

impl Worker {
    /// Serve one pinentry connection to EOF or `BYE`.
    pub fn serve<R, W, F>(
        &mut self,
        reader: &mut R,
        writer: &mut W,
        mut prompt: F,
    ) -> io::Result<()>
    where
        R: BufRead,
        W: Write,
        F: FnMut(Prompt) -> PromptResult,
    {
        send(writer, GREETING)?;
        loop {
            let line = match read_line(reader)? {
                ReadLine::Line(line) => line,
                ReadLine::TooLong => {
                    send(writer, ERR_PARAMETER)?;
                    continue;
                }
                ReadLine::Eof => return Ok(()),
            };
            if line.is_empty() || line.first() == Some(&b'#') {
                continue;
            }
            let (command, argument) = split_command(&line);
            let command = ascii_upper(command);
            if !self.dispatch(&command, argument, writer, &mut prompt)? {
                return Ok(());
            }
        }
    }

    fn dispatch<W, F>(
        &mut self,
        command: &[u8],
        argument: &[u8],
        writer: &mut W,
        prompt: &mut F,
    ) -> io::Result<bool>
    where
        W: Write,
        F: FnMut(Prompt) -> PromptResult,
    {
        match command {
            b"BYE" => {
                send(writer, OK_CLOSING)?;
                Ok(false)
            }
            b"GETPIN" => self.get_pin(writer, prompt).map(|()| true),
            b"CONFIRM" => self.confirm(writer, prompt, argument, false).map(|()| true),
            b"MESSAGE" => self.confirm(writer, prompt, argument, true).map(|()| true),
            b"RESET" => {
                self.state.reset();
                send(writer, OK)?;
                Ok(true)
            }
            b"SETDESC" => set_decoded(&mut self.state.description, argument, writer),
            b"SETPROMPT" => set_decoded(&mut self.state.prompt, argument, writer),
            b"SETTITLE" => set_decoded(&mut self.state.title, argument, writer),
            b"SETERROR" => set_decoded(&mut self.state.error, argument, writer),
            b"SETOK" => set_label(&mut self.state.ok, argument, writer),
            b"SETCANCEL" => set_label(&mut self.state.cancel, argument, writer),
            b"SETTIMEOUT" => {
                let Some(value) = std::str::from_utf8(argument)
                    .ok()
                    .and_then(|text| text.parse::<u64>().ok())
                    .filter(|value| *value <= MAX_TIMEOUT_SECONDS)
                else {
                    send(writer, ERR_PARAMETER)?;
                    return Ok(true);
                };
                self.state.timeout_seconds = value;
                send(writer, OK)?;
                Ok(true)
            }
            b"SETREPEAT" => match percent_decode(argument) {
                Ok(value) => {
                    if let Some(mut old) = self.state.repeat_prompt.replace(value) {
                        wipe(&mut old);
                    }
                    send(writer, OK)?;
                    Ok(true)
                }
                Err(()) => malformed(writer),
            },
            b"SETREPEATERROR" => match percent_decode(argument) {
                Ok(value) => {
                    if let Some(mut old) = self.state.repeat_error.replace(value) {
                        wipe(&mut old);
                    }
                    send(writer, OK)?;
                    Ok(true)
                }
                Err(()) => malformed(writer),
            },
            b"GETINFO" => self.get_info(argument, writer),
            b"OPTION" => self.option(argument, writer),
            b"NOP" | b"SETKEYINFO" | b"SETQUALITYBAR" | b"SETQUALITYBAR_TT" | b"SETGENPIN"
            | b"SETGENPIN_TT" | b"SETREPEATOK" | b"CLEARPASSPHRASE" => {
                send(writer, OK)?;
                Ok(true)
            }
            _ => {
                send(writer, ERR_UNKNOWN)?;
                Ok(true)
            }
        }
    }

    fn option<W: Write>(&mut self, argument: &[u8], writer: &mut W) -> io::Result<bool> {
        if argument == b"putenv=PINENTRY_USER_DATA" {
            wipe(&mut self.state.user_data);
            self.state.user_data.clear();
            send(writer, OK)?;
            return Ok(true);
        }
        let (target, value, label) =
            if let Some(value) = argument.strip_prefix(b"pinentry-user-data=") {
                (&mut self.state.user_data, value, false)
            } else if let Some(value) = argument.strip_prefix(b"putenv=PINENTRY_USER_DATA=") {
                (&mut self.state.user_data, value, false)
            } else if let Some(value) = argument.strip_prefix(b"display=") {
                (&mut self.state.display, value, false)
            } else if let Some(value) = argument.strip_prefix(b"ttyname=") {
                (&mut self.state.ttyname, value, false)
            } else if let Some(value) = argument.strip_prefix(b"ttytype=") {
                (&mut self.state.ttytype, value, false)
            } else if let Some(value) = argument.strip_prefix(b"default-ok=") {
                (&mut self.state.ok, value, true)
            } else if let Some(value) = argument.strip_prefix(b"default-cancel=") {
                (&mut self.state.cancel, value, true)
            } else if let Some(value) = argument.strip_prefix(b"default-prompt=") {
                (&mut self.state.prompt, value, false)
            } else {
                send(writer, OK)?;
                return Ok(true);
            };
        let Ok(mut value) = percent_decode(value) else {
            return malformed(writer);
        };
        if label {
            value = strip_accelerators(value);
        }
        wipe(target);
        *target = value;
        // GnuPG sends display and grab options that do not need rendering
        // changes. Pinentry convention is to acknowledge unsupported options.
        send(writer, OK)?;
        Ok(true)
    }

    fn get_info<W: Write>(&self, argument: &[u8], writer: &mut W) -> io::Result<bool> {
        match argument {
            b"version" => data(writer, b"2.0.0")?,
            b"pid" => data(writer, std::process::id().to_string().as_bytes())?,
            b"flavor" => data(writer, b"topbar")?,
            b"ttyinfo" => {
                let mut fields = Vec::new();
                for field in [
                    &self.state.ttyname,
                    &self.state.ttytype,
                    &self.state.display,
                ] {
                    if field.is_empty() {
                        fields.extend_from_slice(b"-");
                    } else {
                        fields.extend_from_slice(field);
                    }
                    fields.push(b' ');
                }
                fields.pop();
                data(writer, &fields)?;
                wipe(&mut fields);
            }
            _ => {}
        }
        send(writer, OK)?;
        Ok(true)
    }

    fn get_pin<W, F>(&mut self, writer: &mut W, prompt: &mut F) -> io::Result<()>
    where
        W: Write,
        F: FnMut(Prompt) -> PromptResult,
    {
        let repeat = self.state.repeat_prompt.take();
        let repeat_error = self.state.repeat_error.take();
        let mut error = self.state.error.clone();
        loop {
            let first = prompt(Prompt {
                kind: PromptKind::Password,
                context: self.state.context(&self.state.prompt, &error),
                timeout_seconds: self.state.timeout_seconds,
            });
            let PromptResult::Password(first) = first else {
                self.finish_non_password(writer, first)?;
                wipe(&mut self.state.error);
                return Ok(());
            };
            let Some(repeat_prompt) = repeat.as_ref() else {
                if !first.bytes().is_empty() {
                    data(writer, first.bytes())?;
                }
                send(writer, OK)?;
                wipe(&mut self.state.error);
                return Ok(());
            };
            let second = prompt(Prompt {
                kind: PromptKind::Repeat,
                context: self.state.context(
                    if repeat_prompt.is_empty() {
                        DEFAULT_REPEAT_PROMPT
                    } else {
                        repeat_prompt
                    },
                    &error,
                ),
                timeout_seconds: self.state.timeout_seconds,
            });
            let PromptResult::Password(second) = second else {
                self.finish_non_password(writer, second)?;
                wipe(&mut self.state.error);
                return Ok(());
            };
            if first.bytes() == second.bytes() {
                if !first.bytes().is_empty() {
                    data(writer, first.bytes())?;
                }
                send(writer, b"S PIN_REPEATED")?;
                send(writer, OK)?;
                wipe(&mut self.state.error);
                return Ok(());
            }
            wipe(&mut error);
            error.extend_from_slice(repeat_error.as_deref().unwrap_or(DEFAULT_REPEAT_ERROR));
        }
    }

    fn confirm<W, F>(
        &mut self,
        writer: &mut W,
        prompt: &mut F,
        argument: &[u8],
        message: bool,
    ) -> io::Result<()>
    where
        W: Write,
        F: FnMut(Prompt) -> PromptResult,
    {
        let one_button = message
            || argument
                .split(|byte| *byte == b' ')
                .any(|part| part == b"--one-button");
        let kind = if message {
            PromptKind::Message
        } else {
            PromptKind::Confirmation { one_button }
        };
        let result = prompt(Prompt {
            kind,
            context: self.state.context(
                if message { b"Message: " } else { b"Confirm: " },
                &self.state.error,
            ),
            timeout_seconds: self.state.timeout_seconds,
        });
        self.finish_non_password(writer, result)?;
        wipe(&mut self.state.error);
        Ok(())
    }

    fn finish_non_password<W: Write>(
        &self,
        writer: &mut W,
        result: PromptResult,
    ) -> io::Result<()> {
        match result {
            PromptResult::Accepted => send(writer, OK),
            PromptResult::Cancelled | PromptResult::Password(_) => send(writer, ERR_CANCELLED),
            PromptResult::Denied => send(writer, ERR_NOT_CONFIRMED),
            PromptResult::TimedOut => send(writer, ERR_TIMEOUT),
            PromptResult::Failed => send(writer, ERR_PINENTRY),
        }
    }
}

fn malformed<W: Write>(writer: &mut W) -> io::Result<bool> {
    send(writer, ERR_PARAMETER)?;
    Ok(true)
}

fn set_decoded<W: Write>(
    target: &mut Vec<u8>,
    argument: &[u8],
    writer: &mut W,
) -> io::Result<bool> {
    match percent_decode(argument) {
        Ok(value) => {
            wipe(target);
            *target = value;
            send(writer, OK)?;
            Ok(true)
        }
        Err(()) => malformed(writer),
    }
}

fn set_label<W: Write>(target: &mut Vec<u8>, argument: &[u8], writer: &mut W) -> io::Result<bool> {
    match percent_decode(argument) {
        Ok(value) => {
            wipe(target);
            *target = strip_accelerators(value);
            send(writer, OK)?;
            Ok(true)
        }
        Err(()) => malformed(writer),
    }
}

fn send<W: Write>(writer: &mut W, line: &[u8]) -> io::Result<()> {
    writer.write_all(line)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn data<W: Write>(writer: &mut W, value: &[u8]) -> io::Result<()> {
    let mut line = Vec::with_capacity(value.len() + 2);
    line.extend_from_slice(b"D ");
    percent_encode(&mut line, value);
    let result = send(writer, &line);
    wipe(&mut line);
    result
}

/// Erase a byte allocation before releasing it. Volatile stores keep this
/// operation observable to the optimiser; the length is then cleared so a
/// later append cannot expose stale bytes.
fn wipe(value: &mut Vec<u8>) {
    for byte in value.iter_mut() {
        // SAFETY: `byte` is a valid mutable reference into the owned vector.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    value.clear();
}

fn percent_encode(output: &mut Vec<u8>, value: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in value {
        if *byte < 0x20 || *byte == b'%' {
            output.push(b'%');
            output.push(HEX[usize::from(*byte >> 4)]);
            output.push(HEX[usize::from(*byte & 0x0f)]);
        } else {
            output.push(*byte);
        }
    }
}

fn percent_decode(value: &[u8]) -> Result<Vec<u8>, ()> {
    let mut output = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'%' {
            if index + 2 >= value.len() {
                return Err(());
            }
            let high = hex(value[index + 1]).ok_or(())?;
            let low = hex(value[index + 2]).ok_or(())?;
            output.push(high << 4 | low);
            index += 3;
        } else {
            output.push(value[index]);
            index += 1;
        }
    }
    Ok(output)
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn strip_accelerators(value: Vec<u8>) -> Vec<u8> {
    let mut output = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'_' {
            if value.get(index + 1) == Some(&b'_') {
                output.push(b'_');
                index += 2;
            } else {
                index += 1;
            }
        } else {
            output.push(value[index]);
            index += 1;
        }
    }
    output
}

fn split_command(line: &[u8]) -> (&[u8], &[u8]) {
    match line.iter().position(|byte| *byte == b' ') {
        Some(position) => (&line[..position], &line[position + 1..]),
        None => (line, &[]),
    }
}

fn ascii_upper(value: &[u8]) -> Vec<u8> {
    value.iter().map(u8::to_ascii_uppercase).collect()
}

enum ReadLine {
    Eof,
    Line(Vec<u8>),
    TooLong,
}

fn read_line<R: BufRead>(reader: &mut R) -> io::Result<ReadLine> {
    let mut line = Vec::new();
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return if line.is_empty() {
                Ok(ReadLine::Eof)
            } else {
                Ok(ReadLine::Line(line))
            };
        }
        let take = chunk
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(chunk.len(), |at| at + 1);
        if line.len().saturating_add(take) > MAX_LINE_BYTES {
            let ended = chunk.get(take - 1) == Some(&b'\n');
            reader.consume(take);
            if !ended {
                drain_to_newline(reader)?;
            }
            return Ok(ReadLine::TooLong);
        }
        line.extend_from_slice(&chunk[..take]);
        reader.consume(take);
        if line.last() == Some(&b'\n') {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(ReadLine::Line(line));
        }
    }
}

fn drain_to_newline<R: BufRead>(reader: &mut R) -> io::Result<()> {
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return Ok(());
        }
        let take = chunk
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(chunk.len(), |at| at + 1);
        let ended = chunk.get(take - 1) == Some(&b'\n');
        reader.consume(take);
        if ended {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn transcript(input: &[u8], replies: &[PromptResult]) -> String {
        let mut worker = Worker::default();
        let mut input = Cursor::new(input);
        let mut output = Vec::new();
        let mut replies = replies.iter();
        worker
            .serve(&mut input, &mut output, |_| match replies.next() {
                Some(PromptResult::Accepted) => PromptResult::Accepted,
                Some(PromptResult::Cancelled) => PromptResult::Cancelled,
                Some(PromptResult::Denied) => PromptResult::Denied,
                Some(PromptResult::TimedOut) => PromptResult::TimedOut,
                _ => PromptResult::Failed,
            })
            .unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn greeting_reset_info_and_bye_follow_assuan_transcript() {
        assert_eq!(
            transcript(b"GETINFO flavor\nRESET\nBYE\n", &[]),
            "OK Pleased to meet you\nD topbar\nOK\nOK\nOK closing connection\n"
        );
    }

    #[test]
    fn confirmation_denial_and_malformed_escape_are_distinct() {
        assert_eq!(
            transcript(b"CONFIRM\nSETDESC bad%zz\nBOGUS\n", &[PromptResult::Denied]),
            concat!(
                "OK Pleased to meet you\n",
                "ERR 83886194 Not confirmed <Pinentry>\n",
                "ERR 83886360 IPC parameter error <Pinentry>\n",
                "ERR 83886255 Unknown command <Pinentry>\n"
            )
        );
    }

    #[test]
    fn percent_encoding_escapes_protocol_controls() {
        let mut encoded = Vec::new();
        percent_encode(&mut encoded, b"a\n%b");
        assert_eq!(encoded, b"a%0A%25b");
    }

    #[test]
    fn passwords_are_encoded_and_repeat_mismatch_reprompts() {
        let mut worker = Worker::default();
        let mut output = Vec::new();
        let mut replies = [
            b"first".as_slice(),
            b"second".as_slice(),
            b"line\n%".as_slice(),
            b"line\n%".as_slice(),
        ]
        .into_iter();
        let mut saw_repeat_error = false;
        worker
            .serve(
                &mut Cursor::new(b"SETREPEAT\nSETREPEATERROR Mismatch\nGETPIN\n"),
                &mut output,
                |request| {
                    if request.kind == PromptKind::Password
                        && request.context.error.as_slice() == b"Mismatch"
                    {
                        saw_repeat_error = true;
                    }
                    PromptResult::Password(Secret::new(replies.next().unwrap().to_vec()))
                },
            )
            .unwrap();
        assert!(saw_repeat_error);
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "OK Pleased to meet you\nOK\nOK\nD line%0A%25\nS PIN_REPEATED\nOK\n"
        );
    }

    #[test]
    fn timeout_and_reset_do_not_leak_prior_context() {
        let mut worker = Worker::default();
        let mut output = Vec::new();
        let mut contexts = Vec::new();
        worker
            .serve(
                &mut Cursor::new(b"SETTITLE old\nRESET\nSETTIMEOUT 1\nGETPIN\n"),
                &mut output,
                |request| {
                    contexts.push(request.context.title);
                    PromptResult::TimedOut
                },
            )
            .unwrap();
        assert_eq!(contexts, vec![Vec::<u8>::new()]);
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "OK Pleased to meet you\nOK\nOK\nOK\nERR 83886142 Timeout <Pinentry>\n"
        );
    }

    #[test]
    fn impossible_timeout_is_rejected_without_changing_the_previous_deadline() {
        let mut worker = Worker::default();
        let mut output = Vec::new();
        let mut timeout = None;
        worker
            .serve(
                &mut Cursor::new(b"SETTIMEOUT 4\nSETTIMEOUT 18446744073709551615\nGETPIN\n"),
                &mut output,
                |prompt| {
                    timeout = Some(prompt.timeout_seconds);
                    PromptResult::TimedOut
                },
            )
            .unwrap();
        assert_eq!(timeout, Some(4));
        assert_eq!(
            String::from_utf8(output).unwrap(),
            concat!(
                "OK Pleased to meet you\n",
                "OK\n",
                "ERR 83886360 IPC parameter error <Pinentry>\n",
                "ERR 83886142 Timeout <Pinentry>\n"
            )
        );
    }

    #[test]
    fn overlong_lines_are_discarded_without_desynchronising_later_requests() {
        let mut source = vec![b'x'; MAX_LINE_BYTES + 1];
        source.extend_from_slice(b"\nBYE\n");
        let mut worker = Worker::default();
        let mut output = Vec::new();
        worker
            .serve(&mut Cursor::new(source), &mut output, |_| {
                PromptResult::Failed
            })
            .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            concat!(
                "OK Pleased to meet you\n",
                "ERR 83886360 IPC parameter error <Pinentry>\n",
                "OK closing connection\n"
            )
        );
    }
}
