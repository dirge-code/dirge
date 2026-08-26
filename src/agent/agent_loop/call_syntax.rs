//! Where a run of model TEXT stops being prose and becomes a tool call.
//!
//! Some models write their calls into the text channel instead of the
//! structured one — `<tool_call>…</tool_call>` when llama.cpp is served
//! without `--jinja`, `<|DSML|invoke …>` from R1, a ```` ```json ```` fence
//! from anything that learned tool use from a chat log. [`super::scavenge`]
//! exists to dispatch those.
//!
//! This module owns the ONE definition of where such a region starts and
//! ends, because two things need it and they must not disagree:
//!
//!   - the scavenger, deciding what to lift out of the text and run;
//!   - [`DisplayFilter`], deciding what never to show the user.
//!
//! Before this existed only the first had an answer, so the syntax was
//! dispatched AND printed — the turn ran a tool and the user's answer was the
//! call they had written to make it happen (dirge-n00z).
//!
//! # Why a hand-rolled scanner rather than the regexes it replaced
//!
//! The display side sees the text a chunk at a time and cannot unprint. It
//! has to answer a question a whole-text regex never has to: *could this still
//! become a call?* A trailing `` ``` `` is either the start of a fenced call or
//! three backticks, and which one it is has not been written yet. So the
//! scanner reports three outcomes — decided, undecidable-yet, and clear — and
//! the filter holds only on the middle one.

use std::collections::HashSet;
use std::ops::Range;

/// The shapes a call region can take. Both DSML variants dispatch through the
/// same parser; they are separate here only because their closing tags differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionKind {
    /// `<tool_call>…</tool_call>` — Qwen/Hermes' native channel in text form.
    Tagged,
    /// ```` ```json ```` / ```` ```tool ```` fence.
    Fenced,
    /// `<|DSML|invoke name="…">…</|DSML|invoke>`.
    DsmlInvoke,
    /// `<|DSML|function_calls>…</|DSML|function_calls>`, which wraps invokes.
    DsmlFunctionCalls,
}

/// A call region located in some text. Byte offsets into that text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub kind: RegionKind,
    /// The whole region — opening tag through closing tag.
    pub span: Range<usize>,
    /// The payload between them.
    pub body: Range<usize>,
}

/// What the scanner found at the front of the text it was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scan {
    /// No region, and nothing that could grow into one.
    Clear,
    /// Text ran out partway through what might be an opening tag.
    PartialOpener { start: usize },
    /// An opening tag completed, but its closing tag has not arrived.
    Open {
        kind: RegionKind,
        start: usize,
        body_start: usize,
    },
    /// A complete region.
    Region(Region),
}

// ---- opening tags ----

/// Outcome of matching a literal at the front of a slice: matched, ran out of
/// text partway through, or genuinely does not match. The middle case is the
/// whole reason this is not `Option`.
enum Eat<'a> {
    Ok(&'a str),
    Short,
    No,
}

fn eat<'a>(s: &'a str, want: &str) -> Eat<'a> {
    match s.strip_prefix(want) {
        Some(rest) => Eat::Ok(rest),
        // `s` is a proper prefix of what we wanted — more text may complete it.
        None if want.starts_with(s) => Eat::Short,
        None => Eat::No,
    }
}

/// DSML tags are written with either an ASCII `|` or a fullwidth `｜`,
/// sometimes mixed within one tag.
fn eat_pipe(s: &str) -> Eat<'_> {
    match eat(s, "|") {
        Eat::No => eat(s, "｜"),
        other => other,
    }
}

/// `<|DSML|` (or the closing `</|DSML|`), either pipe.
fn eat_dsml_prefix(s: &str, closing: bool) -> Eat<'_> {
    macro_rules! step {
        ($e:expr) => {
            match $e {
                Eat::Ok(r) => r,
                Eat::Short => return Eat::Short,
                Eat::No => return Eat::No,
            }
        };
    }
    let s = step!(eat(s, "<"));
    let s = if closing { step!(eat(s, "/")) } else { s };
    let s = step!(eat_pipe(s));
    let s = step!(eat(s, "DSML"));
    eat_pipe(s)
}

/// What sits at the front of a slice.
enum Opener {
    No,
    /// Could still become an opening tag with more text.
    Partial,
    Complete {
        kind: RegionKind,
        /// Length of the opening tag.
        len: usize,
    },
}

macro_rules! step {
    ($e:expr) => {
        match $e {
            Eat::Ok(r) => r,
            Eat::Short => return Opener::Partial,
            Eat::No => return Opener::No,
        }
    };
}

fn tagged_opener(s: &str) -> Opener {
    const OPEN: &str = "<tool_call>";
    step!(eat(s, OPEN));
    Opener::Complete {
        kind: RegionKind::Tagged,
        len: OPEN.len(),
    }
}

/// ```` ```json ```` / ```` ```tool ```` followed by the rest of its line.
///
/// A bare ```` ``` ```` fence is deliberately not an opener: those are
/// overwhelmingly code the model is discussing. The language tag is what makes
/// the region a claim about being a call.
fn fenced_opener(s: &str) -> Opener {
    let mut partial = false;
    for lang in ["```json", "```tool"] {
        let rest = match eat(s, lang) {
            Eat::Ok(r) => r,
            Eat::Short => {
                partial = true;
                continue;
            }
            Eat::No => continue,
        };
        // The rest of the tag line: spaces, then the newline that starts the
        // payload. Anything else means this was ```` ```jsonc ```` or similar.
        let after = rest.trim_start_matches([' ', '\t', '\r']);
        match after.strip_prefix('\n') {
            Some(body) => {
                return Opener::Complete {
                    kind: RegionKind::Fenced,
                    len: s.len() - body.len(),
                };
            }
            // Ran out before the newline — undecided, not a miss.
            None if after.is_empty() => partial = true,
            None => {}
        }
    }
    if partial { Opener::Partial } else { Opener::No }
}

fn dsml_opener(s: &str) -> Opener {
    let rest = step!(eat_dsml_prefix(s, false));
    // The wrapper first: it is a strict prefix relationship with nothing, so
    // order between the two only matters for which `Partial` we report.
    match eat(rest, "function_calls>") {
        Eat::Ok(r) => {
            return Opener::Complete {
                kind: RegionKind::DsmlFunctionCalls,
                len: s.len() - r.len(),
            };
        }
        Eat::Short => return Opener::Partial,
        Eat::No => {}
    }
    let rest = step!(eat(rest, "invoke"));
    // `<|DSML|invoke name="…">` — attributes run to the first `>`.
    if !rest.starts_with([' ', '\t', '\n', '\r']) {
        return if rest.is_empty() {
            Opener::Partial
        } else {
            Opener::No
        };
    }
    match rest.find('>') {
        Some(gt) => Opener::Complete {
            kind: RegionKind::DsmlInvoke,
            len: s.len() - rest.len() + gt + 1,
        },
        None => Opener::Partial,
    }
}

/// The opening tag at the front of `s`, if any. A completed tag beats an
/// undecided one; an undecided one beats nothing.
fn opener_at(s: &str) -> Opener {
    let mut partial = false;
    for probe in [tagged_opener, fenced_opener, dsml_opener] {
        match probe(s) {
            Opener::Complete { kind, len } => return Opener::Complete { kind, len },
            Opener::Partial => partial = true,
            Opener::No => {}
        }
    }
    if partial { Opener::Partial } else { Opener::No }
}

// ---- closing tags ----

/// Offset and length of `kind`'s closing tag in `s`, searching from the front.
fn find_closer(kind: RegionKind, s: &str) -> Option<(usize, usize)> {
    match kind {
        RegionKind::Tagged => s.find("</tool_call>").map(|i| (i, "</tool_call>".len())),
        RegionKind::Fenced => s.find("\n```").map(|i| (i, "\n```".len())),
        RegionKind::DsmlInvoke => find_dsml_closer(s, "invoke>", false),
        // The scavenger's wrapper pattern accepts a closing tag with the
        // slash missing, which is how R1 actually writes it often enough to
        // matter. Kept bug-for-bug so the two agree.
        RegionKind::DsmlFunctionCalls => find_dsml_closer(s, "function_calls>", true),
    }
}

fn find_dsml_closer(s: &str, tail: &str, slash_optional: bool) -> Option<(usize, usize)> {
    for (i, _) in s.char_indices() {
        for closing in [true, false] {
            if !closing && !slash_optional {
                continue;
            }
            if let Eat::Ok(rest) = eat_dsml_prefix(&s[i..], closing)
                && let Eat::Ok(after) = eat(rest, tail)
            {
                return Some((i, s.len() - i - after.len()));
            }
        }
    }
    None
}

// ---- scanning ----

/// The first call region at or after the front of `text`.
///
/// `eof_closes` says whether the text is complete. Mid-stream it is `false`,
/// so a region whose closing tag has not arrived reports [`Scan::Open`] and
/// the caller waits. On a finished message it is `true`, and the kinds the
/// scavenger repairs ([`RegionKind::closed_by_eof`]) run to the end.
pub fn scan(text: &str, eof_closes: bool) -> Scan {
    for (i, _) in text.char_indices() {
        let (kind, len) = match opener_at(&text[i..]) {
            Opener::No => continue,
            Opener::Partial => return Scan::PartialOpener { start: i },
            Opener::Complete { kind, len } => (kind, len),
        };
        let body_start = i + len;
        return match find_closer(kind, &text[body_start..]) {
            Some((rel, closer_len)) => Scan::Region(Region {
                kind,
                span: i..body_start + rel + closer_len,
                body: body_start..body_start + rel,
            }),
            // End of a finished message closes whatever is open, for EVERY
            // kind — [`is_call`] then decides, on its own, whether the
            // scavenger will run it. Making the region conditional on the
            // kind as well encoded the same rule twice and got it wrong: a
            // `function_calls` wrapper cut off after a COMPLETE inner invoke
            // dispatches (the scavenger reads invokes with its own regex,
            // wrapper or no wrapper) but was left on screen.
            None if eof_closes => Scan::Region(Region {
                kind,
                span: i..text.len(),
                body: body_start..text.len(),
            }),
            None => Scan::Open {
                kind,
                start: i,
                body_start,
            },
        };
    }
    Scan::Clear
}

/// Every call region in a COMPLETE text, in the order they appear.
///
/// This is the scavenger's view: it holds the whole message, so an
/// unterminated region is a truncated call rather than one still arriving.
pub fn regions(text: &str) -> Vec<Region> {
    let mut out = Vec::new();
    let mut base = 0;
    while base < text.len() {
        let Scan::Region(r) = scan(&text[base..], true) else {
            break;
        };
        let end = base + r.span.end;
        out.push(Region {
            kind: r.kind,
            span: base + r.span.start..end,
            body: base + r.body.start..base + r.body.end,
        });
        // An empty advance would spin; openers are non-empty so this is
        // belt-and-braces, not a live case.
        if end <= base {
            break;
        }
        base = end;
    }
    out
}

/// `text` with every region in `spans` cut out, the surroundings joined.
pub fn without(text: &str, regions: &[Region]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for r in regions {
        out.push_str(&text[at..r.span.start]);
        at = r.span.end;
    }
    out.push_str(&text[at..]);
    out
}

/// Whether this region will actually reach a tool.
///
/// The same predicate the scavenger drops on, so display and dispatch cannot
/// disagree about what happened: a region that runs is hidden, and one that
/// does not is left on screen. Leaving it there is deliberate — a call naming
/// a tool that does not exist produces no result and no error (dirge-knt8), so
/// hiding it too would end the turn with nothing at all for the user to read.
pub fn is_call(text: &str, region: &Region, allowed: &HashSet<String>) -> bool {
    match region.kind {
        RegionKind::Tagged | RegionKind::Fenced => {
            super::scavenge::coerce_to_tool_call(text[region.body.clone()].trim(), allowed)
                .is_some()
        }
        RegionKind::DsmlInvoke | RegionKind::DsmlFunctionCalls => {
            super::scavenge::iterate_dsml_invokes(&text[region.span.clone()])
                .iter()
                .any(|i| super::scavenge::is_dispatchable(&i.name, allowed))
        }
    }
}

/// Rewrite an assistant message so that calls the loop lifted out of its TEXT
/// are recorded as calls: the syntax leaves the text, and a real tool-call
/// block takes its place.
///
/// Without this the transcript claims the model wrote prose and then a tool
/// result appears from nowhere. Three things go wrong, and the first two are
/// not cosmetic:
///
///   - **The next request is malformed.** A `role: "tool"` message with no
///     preceding `tool_calls` is a hard 400 on OpenAI and Anthropic. It has
///     stayed latent only because text-channel calls come from servers
///     lenient enough to have leaked them in the first place.
///   - **Results get crossed.** Scavenged calls carried an empty id, so two in
///     one turn were indistinguishable — result-to-call matching, the storm
///     signature, and the publish guard's id filter all resolved to whichever
///     came first.
///   - **The model reads its own leak back as prose**, which is the shape it
///     just got a result for, so it writes another one.
///
/// Text blocks only. Reasoning is a separate channel most providers drop on
/// replay, and rewriting it risks the signature checks on providers that
/// don't.
pub fn absorb_text_calls(
    msg: &super::message::AssistantMessage,
    calls: &[super::tools::ToolCall],
    allowed: &HashSet<String>,
) -> super::message::AssistantMessage {
    use super::message::ContentBlock;
    let mut content: Vec<ContentBlock> = Vec::with_capacity(msg.content.len() + calls.len());
    for block in &msg.content {
        let ContentBlock::Text { text } = block else {
            content.push(block.clone());
            continue;
        };
        let kept: Vec<Region> = regions(text)
            .into_iter()
            .filter(|r| is_call(text, r, allowed))
            .collect();
        let stripped = without(text, &kept);
        // An all-syntax block leaves nothing behind, and an empty text block
        // is rejected outright by several providers (dirge-byun).
        if !stripped.trim().is_empty() {
            content.push(ContentBlock::Text { text: stripped });
        }
    }
    content.extend(calls.iter().map(|c| ContentBlock::ToolCall {
        id: c.id.clone(),
        name: c.name.clone(),
        arguments: c.arguments.clone(),
    }));
    super::message::AssistantMessage {
        content,
        stop_reason: msg.stop_reason,
        error_message: msg.error_message.clone(),
    }
}

// ---- the display filter ----

/// Withhold the text of calls the model wrote into its answer.
///
/// Fed the assistant's text in arrival order; returns the part that is safe to
/// show now. Text outside a call region passes straight through, so ordinary
/// streaming is unaffected apart from a few bytes of lag when the model types
/// something that starts like an opening tag.
///
/// Order is preserved: once a region opens, everything after it is withheld
/// too, until the region resolves. The alternative — releasing the tail and
/// the region separately — can only put them on screen out of order.
#[derive(Debug, Default)]
pub struct DisplayFilter {
    allowed: HashSet<String>,
    /// Text received but not yet released.
    held: String,
    /// Set once a region has opened; the closing-tag search resumes here so a
    /// long region is not re-scanned from the front on every chunk.
    open: Option<OpenRegion>,
}

#[derive(Debug, Clone, Copy)]
struct OpenRegion {
    kind: RegionKind,
    body_start: usize,
    search_from: usize,
}

/// Longest closing tag, so a resumed search overlaps enough to catch one split
/// across two chunks.
const MAX_CLOSER_LEN: usize = 24;

impl DisplayFilter {
    pub fn new(allowed: HashSet<String>) -> Self {
        Self {
            allowed,
            held: String::new(),
            open: None,
        }
    }

    /// Feed the next chunk of assistant text; returns what may be shown now.
    pub fn push(&mut self, chunk: &str) -> String {
        self.held.push_str(chunk);
        let mut out = String::new();
        loop {
            if let Some(open) = self.open {
                match find_closer(open.kind, &self.held[open.search_from..]) {
                    Some((rel, len)) => {
                        let end = open.search_from + rel + len;
                        let region = Region {
                            kind: open.kind,
                            span: 0..end,
                            body: open.body_start..open.search_from + rel,
                        };
                        if !is_call(&self.held, &region, &self.allowed) {
                            out.push_str(&self.held[..end]);
                        }
                        self.held.drain(..end);
                        self.open = None;
                        continue;
                    }
                    None => {
                        self.open = Some(OpenRegion {
                            search_from: resume_from(&self.held),
                            ..open
                        });
                        break;
                    }
                }
            }
            match scan(&self.held, false) {
                Scan::Clear => {
                    out.push_str(&self.held);
                    self.held.clear();
                    break;
                }
                Scan::PartialOpener { start } => {
                    out.push_str(&self.held[..start]);
                    self.held.drain(..start);
                    break;
                }
                Scan::Open {
                    kind,
                    start,
                    body_start,
                } => {
                    out.push_str(&self.held[..start]);
                    self.held.drain(..start);
                    self.open = Some(OpenRegion {
                        kind,
                        body_start: body_start - start,
                        search_from: body_start - start,
                    });
                    continue;
                }
                Scan::Region(r) => {
                    out.push_str(&self.held[..r.span.start]);
                    if !is_call(&self.held, &r, &self.allowed) {
                        out.push_str(&self.held[r.span.clone()]);
                    }
                    let end = r.span.end;
                    self.held.drain(..end);
                    continue;
                }
            }
        }
        out
    }

    /// The message is over. Releases everything still withheld, minus any
    /// truncated region the scavenger will repair and run.
    pub fn flush(&mut self) -> String {
        self.open = None;
        let held = std::mem::take(&mut self.held);
        let mut out = String::new();
        let mut at = 0;
        while at < held.len() {
            let Scan::Region(r) = scan(&held[at..], true) else {
                break;
            };
            out.push_str(&held[at..at + r.span.start]);
            let region = Region {
                kind: r.kind,
                span: at + r.span.start..at + r.span.end,
                body: at + r.body.start..at + r.body.end,
            };
            if !is_call(&held, &region, &self.allowed) {
                out.push_str(&held[region.span.clone()]);
            }
            at = region.span.end;
        }
        out.push_str(&held[at..]);
        out
    }

    /// Forget any partially-seen message. Called at a turn boundary, where
    /// the next message's text has nothing to do with the last one's.
    pub fn reset(&mut self) {
        self.held.clear();
        self.open = None;
    }

    /// The same decisions applied to a message already complete in hand,
    /// without disturbing anything this filter is mid-way through.
    ///
    /// Equivalent to feeding the text through [`push`](Self::push) and
    /// [`flush`](Self::flush) — pinned by a test, because a caller that holds
    /// the whole message and one that watches it arrive must not be able to
    /// show the user different things.
    pub fn strip(&self, text: &str) -> String {
        let mut f = DisplayFilter::new(self.allowed.clone());
        let mut out = f.push(text);
        out.push_str(&f.flush());
        out
    }
}

/// Where to resume a closing-tag search after a fruitless pass, keeping enough
/// overlap that a tag split across two chunks is still found.
fn resume_from(held: &str) -> usize {
    let want = held.len().saturating_sub(MAX_CLOSER_LEN);
    (0..=want)
        .rev()
        .find(|i| held.is_char_boundary(*i))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn shown(chunks: &[&str], allowed: &[&str]) -> String {
        let mut f = DisplayFilter::new(tools(allowed));
        let mut out = String::new();
        for c in chunks {
            out.push_str(&f.push(c));
        }
        out.push_str(&f.flush());
        out
    }

    /// Split `text` every `n` bytes (on char boundaries) so one fixture can be
    /// replayed at every chunking a provider might produce.
    fn chunked(text: &str, n: usize) -> Vec<&str> {
        let mut out = Vec::new();
        let mut start = 0;
        while start < text.len() {
            let mut end = (start + n).min(text.len());
            while !text.is_char_boundary(end) {
                end += 1;
            }
            out.push(&text[start..end]);
            start = end;
        }
        out
    }

    // ---- the negative half, written first: what must never be withheld ----

    /// Ordinary prose is untouched at every chunk size, byte for byte. The
    /// filter sits on the streaming path of every turn, so the case that
    /// matters most is the one where it has nothing to do.
    #[test]
    fn prose_passes_through_unchanged() {
        for text in [
            "Here is the answer.",
            "Use `bash` for that — see the ``` fence below.",
            "```\n{\"name\": \"bash\"}\n```",
            "```rust\nfn main() {}\n```",
            "I would call bash with {\"name\": \"bash\", \"arguments\": {}}.",
            "A stray < and a lone ``` and a | pipe.",
            "<tool_output>not an opener</tool_output>",
        ] {
            for n in [1, 3, 7, 512] {
                assert_eq!(
                    shown(&chunked(text, n), &["bash", "read"]),
                    text,
                    "chunk size {n}: {text}"
                );
            }
        }
    }

    /// A fence the model is SHOWING the user, not calling. `coerce_to_tool_call`
    /// is what separates the two, and it must be consulted for fences or every
    /// ```json block in an answer disappears.
    #[test]
    fn a_json_fence_that_is_not_a_call_is_shown() {
        let text = "Config:\n```json\n{\"port\": 8080}\n```\nThat's it.";
        assert_eq!(shown(&chunked(text, 5), &["bash"]), text);
    }

    /// The dirge-knt8 rule, on the display side: a call naming a tool that
    /// does not exist produces no result and no error, so the turn ends here.
    /// Hiding the syntax as well would leave the user an empty answer.
    #[test]
    fn a_call_naming_no_tool_is_left_on_screen() {
        let text = "<tool_call>\n{\"name\": \"frobnicate\", \"arguments\": {}}\n</tool_call>";
        assert_eq!(shown(&chunked(text, 4), &["bash"]), text);
    }

    // ---- the positive half ----

    /// dirge-n00z, exactly as reproduced against the real binary.
    #[test]
    fn a_dispatched_tagged_call_is_withheld() {
        let text = "<tool_call>\n{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"}}\n</tool_call>DONE";
        for n in [1, 2, 5, 13, 512] {
            assert_eq!(
                shown(&chunked(text, n), &["bash"]),
                "DONE",
                "chunk size {n}"
            );
        }
    }

    #[test]
    fn a_dispatched_fenced_call_is_withheld() {
        let text = "Running it.\n```json\n{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"}}\n```\n";
        assert_eq!(shown(&chunked(text, 6), &["bash"]), "Running it.\n\n");
    }

    #[test]
    fn a_dispatched_dsml_call_is_withheld() {
        let text = "Editing.\n<|DSML|invoke name=\"bash\">\
                    <|DSML|parameter name=\"command\" string=\"true\">ls</|DSML|parameter>\
                    </|DSML|invoke>\nDone.";
        assert_eq!(shown(&chunked(text, 9), &["bash"]), "Editing.\n\nDone.");
    }

    #[test]
    fn a_dsml_function_calls_wrapper_is_withheld_whole() {
        let text = "<｜DSML｜function_calls> <｜DSML｜invoke name=\"bash\">\
                    <｜DSML｜parameter name=\"command\" string=\"true\">ls</｜DSML｜parameter>\
                    </｜DSML｜invoke> </｜DSML｜function_calls>ok";
        assert_eq!(shown(&chunked(text, 11), &["bash"]), "ok");
    }

    /// A name the alias table places counts as dispatched — the scavenger's
    /// gate accepts it, so the display must hide it too (dirge-e31n.8).
    #[test]
    fn an_aliased_name_is_withheld_because_it_dispatches() {
        let text =
            "<tool_call>{\"name\": \"shell\", \"arguments\": {\"command\": \"ls\"}}</tool_call>";
        assert_eq!(shown(&chunked(text, 3), &["bash"]), "");
    }

    /// A call cut off at `max_tokens`. The scavenger repairs and runs the
    /// tagged and fenced shapes, so the display has to hide them; it does not
    /// repair DSML, so that one is shown.
    #[test]
    fn truncation_is_handled_the_way_the_scavenger_handles_it() {
        let runs = "<tool_call>{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"";
        assert_eq!(shown(&chunked(runs, 4), &["bash"]), "");

        let shows = "<|DSML|invoke name=\"bash\"><|DSML|parameter name=\"command\">ls";
        assert_eq!(shown(&chunked(shows, 4), &["bash"]), shows);
    }

    /// Two calls in one message, with prose between and after.
    #[test]
    fn several_regions_in_one_message() {
        let text = "first <tool_call>{\"name\": \"bash\", \"arguments\": {}}</tool_call> \
                    second <tool_call>{\"name\": \"read\", \"arguments\": {}}</tool_call> end";
        assert_eq!(
            shown(&chunked(text, 7), &["bash", "read"]),
            "first  second  end"
        );
    }

    /// Order is the point of holding the tail: the text after a region must
    /// not overtake the region while it is still undecided.
    #[test]
    fn text_after_an_undecided_region_does_not_overtake_it() {
        let mut f = DisplayFilter::new(tools(&["bash"]));
        assert_eq!(f.push("intro <tool_call>{\"name\": \"nope\"}"), "intro ");
        // Region still open — the tail is held, not emitted ahead of it.
        assert_eq!(f.push(" trailing"), "");
        // Region decided — it and everything queued behind it are released,
        // in the order they were written.
        assert_eq!(
            f.push("</tool_call>tail"),
            "<tool_call>{\"name\": \"nope\"} trailing</tool_call>tail"
        );
        assert_eq!(f.flush(), "");
    }

    /// A closing tag arriving one byte at a time still terminates the region:
    /// the resumed search overlaps by more than the longest tag.
    #[test]
    fn a_closing_tag_split_across_chunks_is_still_found() {
        let text = "<tool_call>{\"name\": \"bash\", \"arguments\": {}}</tool_call>after";
        assert_eq!(shown(&chunked(text, 1), &["bash"]), "after");
    }

    // ---- the scanner itself ----

    /// A fence with no language tag, or with one that merely starts like
    /// `json`, is code the model is showing — not a call. (The trailing `x`
    /// is what decides the closing fence; without it the text ends mid-way
    /// through something that could still become ```` ```json ````.)
    #[test]
    fn a_bare_fence_is_not_an_opener() {
        assert_eq!(scan("```\n{}\n```x", false), Scan::Clear);
        assert_eq!(scan("```jsonc\n{}\n```x", false), Scan::Clear);
        assert!(regions("```\n{}\n```").is_empty());
        assert!(regions("```jsonc\n{}\n```").is_empty());
    }

    #[test]
    fn an_unfinished_opener_is_undecided_not_clear() {
        for prefix in ["<tool_", "``", "```js", "<|DSM", "<|DSML|invoke"] {
            assert!(
                matches!(
                    scan(prefix, false),
                    Scan::PartialOpener { .. } | Scan::Open { .. }
                ),
                "{prefix} was decided too early",
            );
        }
    }

    #[test]
    fn regions_are_found_in_order_and_cut_out() {
        let text = "a<tool_call>X</tool_call>b```json\nY\n```c";
        let found = regions(text);
        assert_eq!(
            found.iter().map(|r| r.kind).collect::<Vec<_>>(),
            vec![RegionKind::Tagged, RegionKind::Fenced],
        );
        assert_eq!(&text[found[0].body.clone()], "X");
        assert_eq!(&text[found[1].body.clone()], "Y");
        assert_eq!(without(text, &found), "abc");
    }

    /// The scavenger reads the whole message, so an unterminated region is a
    /// truncated call — not one still arriving.
    #[test]
    fn a_complete_text_closes_its_last_region_at_the_end() {
        let found = regions("<tool_call>{\"name\": \"bash\"");
        assert_eq!(found.len(), 1);
        assert_eq!(
            &"<tool_call>{\"name\": \"bash\""[found[0].body.clone()],
            "{\"name\": \"bash\""
        );

        // Every kind, so a region that runs cannot slip past by arriving
        // without its closing tag.
        assert_eq!(regions("<|DSML|invoke name=\"bash\">oops").len(), 1);
    }

    /// THE invariant, stated directly against the thing it has to agree with:
    /// a region is hidden exactly when the scavenger lifts a call out of it.
    ///
    /// Everything else in this module is a means to this. Written against
    /// `scavenge_tool_calls` itself rather than a second opinion about what it
    /// does, because a copy of its rules is what the bug was made of.
    ///
    /// Bare JSON in prose is out of scope on purpose: the scavenger does
    /// dispatch it, but it is not call SYNTAX — a model quoting a schema
    /// writes the same characters — so it stays on screen.
    #[test]
    fn a_region_is_hidden_exactly_when_it_dispatches() {
        let allowed = tools(&["bash", "read"]);
        for text in [
            "<tool_call>{\"name\": \"bash\", \"arguments\": {}}</tool_call>",
            "<tool_call>{\"name\": \"nope\", \"arguments\": {}}</tool_call>",
            "<tool_call>{\"name\": \"shell\", \"arguments\": {}}</tool_call>",
            "<tool_call>{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"",
            "```json\n{\"name\": \"read\", \"arguments\": {\"path\": \"x\"}}\n```",
            "```json\n{\"port\": 8080}\n```",
            "<|DSML|invoke name=\"bash\"><|DSML|parameter name=\"command\">ls\
             </|DSML|parameter></|DSML|invoke>",
            "<|DSML|invoke name=\"nope\"><|DSML|parameter name=\"x\">y\
             </|DSML|parameter></|DSML|invoke>",
            "<|DSML|invoke name=\"bash\">cut off before the closing tag",
            // A wrapper cut off after a complete invoke: the scavenger reads
            // the invoke and runs it, so the syntax must not be shown.
            "<|DSML|function_calls><|DSML|invoke name=\"bash\">\
             <|DSML|parameter name=\"command\">ls</|DSML|parameter></|DSML|invoke>",
        ] {
            let found = regions(text);
            assert_eq!(found.len(), 1, "expected one region in: {text}");
            let region = &found[0];
            let dispatches = !super::super::scavenge::scavenge_tool_calls(
                Some(&text[region.span.clone()]),
                &allowed,
                4,
            )
            .calls
            .is_empty();
            assert_eq!(
                is_call(text, region, &allowed),
                dispatches,
                "display and dispatch disagree about: {text}",
            );
        }
    }

    /// The closing tag is found while the message is still streaming, not
    /// deferred to the flush. Nothing about the final text says which, and the
    /// difference is whether the rest of a long turn appears as it is written
    /// or all at once when the model stops.
    #[test]
    fn a_region_closes_while_the_message_is_still_arriving() {
        let mut f = DisplayFilter::new(tools(&["bash"]));
        let text = "<tool_call>{\"name\": \"bash\", \"arguments\": {}}</tool_call>after";
        let mut out = String::new();
        for c in chunked(text, 1) {
            out.push_str(&f.push(c));
        }
        assert_eq!(out, "after", "the tail should stream, not wait for flush");
        assert_eq!(f.flush(), "");
    }

    /// Two callers read this filter — one watching the message arrive, one
    /// holding it finished — and they must reach the same text. They are
    /// separate code paths ([`DisplayFilter::push`] plus
    /// [`DisplayFilter::flush`], against [`DisplayFilter::strip`]), so nothing
    /// but this pins them together; drift here shows up as the streamed answer
    /// and the final answer disagreeing, which is what dirge-n00z looked like.
    #[test]
    fn watching_a_message_arrive_and_reading_it_whole_agree() {
        let allowed = tools(&["bash", "read"]);
        let whole = DisplayFilter::new(allowed.clone());
        for text in [
            "plain prose",
            "<tool_call>{\"name\": \"bash\", \"arguments\": {}}</tool_call>tail",
            "<tool_call>{\"name\": \"nope\", \"arguments\": {}}</tool_call>tail",
            "a ```json\n{\"name\": \"bash\", \"arguments\": {}}\n``` b",
            "a ```json\n{\"port\": 1}\n``` b",
            "truncated <tool_call>{\"name\": \"bash\"",
            "truncated <|DSML|invoke name=\"bash\">oops",
            "```\nplain fence\n```",
            "trailing backticks ``",
        ] {
            for n in [1, 2, 5, 31] {
                assert_eq!(
                    shown(&chunked(text, n), &["bash", "read"]),
                    whole.strip(text),
                    "chunk size {n}: {text}",
                );
            }
        }
    }

    // ---- recording a lifted call on the message that made it ----

    fn assistant(text: &str) -> super::super::message::AssistantMessage {
        super::super::message::AssistantMessage::new(
            vec![super::super::message::ContentBlock::Text {
                text: text.to_string(),
            }],
            super::super::message::StopReason::ToolUse,
        )
    }

    fn call(id: &str, name: &str) -> super::super::tools::ToolCall {
        super::super::tools::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({"command": "ls"}),
        }
    }

    /// The syntax leaves the text and a real call takes its place, so the next
    /// request carries a `tool_use` for the `tool_result` that follows it.
    #[test]
    fn a_lifted_call_is_recorded_on_the_assistant_message() {
        use super::super::message::ContentBlock;
        let msg = assistant(
            "sure <tool_call>{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"}}</tool_call> ok",
        );
        let out = absorb_text_calls(&msg, &[call("scav-1", "bash")], &tools(&["bash"]));
        match out.content.as_slice() {
            [
                ContentBlock::Text { text },
                ContentBlock::ToolCall { id, name, .. },
            ] => {
                assert_eq!(text, "sure  ok");
                assert_eq!(id, "scav-1");
                assert_eq!(name, "bash");
            }
            other => panic!("{other:?}"),
        }
    }

    /// A turn that was nothing but call syntax leaves no text block at all —
    /// an empty one is rejected outright by several providers.
    #[test]
    fn an_all_syntax_turn_leaves_no_empty_text_block() {
        use super::super::message::ContentBlock;
        let msg = assistant("<tool_call>{\"name\": \"bash\", \"arguments\": {}}</tool_call>");
        let out = absorb_text_calls(&msg, &[call("scav-1", "bash")], &tools(&["bash"]));
        assert!(
            matches!(out.content.as_slice(), [ContentBlock::ToolCall { .. }]),
            "{:?}",
            out.content,
        );
    }

    /// The other direction: syntax that dispatched nothing is the model's
    /// words and stays in them. Same predicate as the display filter, so the
    /// user and the model are shown the same message.
    #[test]
    fn syntax_that_dispatched_nothing_stays_in_the_text() {
        use super::super::message::ContentBlock;
        let leak = "<tool_call>{\"name\": \"frobnicate\", \"arguments\": {}}</tool_call>";
        let msg = assistant(leak);
        let out = absorb_text_calls(&msg, &[], &tools(&["bash"]));
        match out.content.as_slice() {
            [ContentBlock::Text { text }] => assert_eq!(text, leak),
            other => panic!("{other:?}"),
        }
    }

    /// Reasoning is a separate channel — providers drop it on replay and some
    /// validate it — so it is left exactly as the model wrote it.
    #[test]
    fn reasoning_content_is_not_rewritten() {
        use super::super::message::ContentBlock;
        let thought = "<tool_call>{\"name\": \"bash\", \"arguments\": {}}</tool_call>";
        let msg = super::super::message::AssistantMessage::new(
            vec![ContentBlock::Thinking {
                text: thought.to_string(),
                signature: None,
                signature_model: None,
            }],
            super::super::message::StopReason::ToolUse,
        );
        let out = absorb_text_calls(&msg, &[call("scav-1", "bash")], &tools(&["bash"]));
        match out.content.as_slice() {
            [
                ContentBlock::Thinking { text, .. },
                ContentBlock::ToolCall { .. },
            ] => {
                assert_eq!(text, thought)
            }
            other => panic!("{other:?}"),
        }
    }

    /// Multi-byte text must not panic the byte-offset arithmetic, and must
    /// come out the other side intact.
    #[test]
    fn multibyte_text_survives() {
        let text = "答え → <tool_call>{\"name\": \"bash\", \"arguments\": {}}</tool_call> ✓";
        assert_eq!(shown(&chunked(text, 2), &["bash"]), "答え →  ✓");
    }
}
