use crate::{
    cst::{CstNode, Node, NodeKind},
    lexer::{Token, TokenKind},
};

#[derive(Debug, Clone)]
pub struct FmtOptions {
    pub eol_last: bool,
    pub indent: usize,
    pub line_width: usize,
}

impl Default for FmtOptions {
    fn default() -> Self {
        Self {
            eol_last: false,
            indent: 2,
            line_width: 120,
        }
    }
}

// Forward declaration for trait object
trait TextSpan: std::fmt::Debug {
    fn is_empty(&self) -> bool;
    fn length(&self) -> usize;
    fn format(
        &mut self,
        w: &mut LineWriter,
        depth: usize,
        options: &FmtOptions,
        parent: &dyn Rule,
    ) -> bool;
    fn first_line_length(&self, options: &FmtOptions, parent: &dyn Rule) -> usize;
}

#[derive(Debug)]
struct Chunk {
    text: String,
    force_newline: bool,
}

impl Chunk {
    fn new(text: String, force_newline: bool) -> Self {
        Self {
            text,
            force_newline,
        }
    }

    fn append(&mut self, text: &str) {
        self.text.push_str(text);
    }
}

impl TextSpan for Chunk {
    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn length(&self) -> usize {
        self.text.len()
    }

    fn format(
        &mut self,
        w: &mut LineWriter,
        depth: usize,
        _options: &FmtOptions,
        _parent: &dyn Rule,
    ) -> bool {
        if self.force_newline {
            w.write(&self.text);
            w.newline();
            w.indent(depth);
            return true;
        }
        w.write(&self.text);
        false
    }

    fn first_line_length(&self, _options: &FmtOptions, _parent: &dyn Rule) -> usize {
        self.length()
    }
}

#[derive(Debug)]
struct Group {
    rule: Box<dyn Rule>,
    depth: usize,
    spans: Vec<Box<dyn TextSpan>>,
}

impl Group {
    fn new(rule: Box<dyn Rule>, depth: usize) -> Self {
        Self {
            rule,
            depth,
            spans: Vec::new(),
        }
    }

    fn last(&self) -> Option<&Box<dyn TextSpan>> {
        self.spans.last()
    }

    fn write(&mut self, text: String, force_newline: bool) -> &mut Self {
        let chunk = Box::new(Chunk::new(text, force_newline));
        self.spans.push(chunk);
        self
    }

    fn new_chunk(&mut self) -> &mut Self {
        self.write(String::new(), false)
    }

    fn push(&mut self, group: Box<dyn TextSpan>) {
        self.spans.push(group);
    }

    fn close(&mut self) {
        // Remove trailing empty spans
        while let Some(last) = self.spans.last() {
            if last.is_empty() {
                self.spans.pop();
            } else {
                break;
            }
        }
    }
}

impl TextSpan for Group {
    fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    fn length(&self) -> usize {
        self.spans.iter().map(|span| span.length()).sum()
    }

    fn format(
        &mut self,
        w: &mut LineWriter,
        _depth: usize,
        options: &FmtOptions,
        parent: &dyn Rule,
    ) -> bool {
        // Check if we need to split more
        let max_kinds = self.rule.nb_kinds();
        while self.rule.kind() < max_kinds - 1 {
            let len = w.line_length() + self.rule.first_line_length(&self.spans, options, parent);
            if len > options.line_width {
                self.rule.increment_kind();
            } else {
                break;
            }
        }

        self.rule
            .format(w, self.depth, &mut self.spans, options, parent)
    }

    fn first_line_length(&self, options: &FmtOptions, parent: &dyn Rule) -> usize {
        self.rule.first_line_length(&self.spans, options, parent)
    }
}

trait Rule: std::fmt::Debug {
    fn kind(&self) -> usize;
    fn increment_kind(&mut self);
    fn nb_kinds(&self) -> usize {
        1
    }

    fn format(
        &mut self,
        w: &mut LineWriter,
        depth: usize,
        spans: &mut [Box<dyn TextSpan>],
        options: &FmtOptions,
        parent: &dyn Rule,
    ) -> bool;

    fn first_line_length(
        &self,
        spans: &[Box<dyn TextSpan>],
        options: &FmtOptions,
        _parent: &dyn Rule,
    ) -> usize
    where
        Self: Sized,
    {
        if spans.is_empty() {
            return 0;
        }
        spans[0].first_line_length(options, self)
    }
}

#[derive(Debug)]
struct DefaultRule {
    kind_val: usize,
}

impl DefaultRule {
    fn new() -> Self {
        Self { kind_val: 0 }
    }
}

impl Rule for DefaultRule {
    fn kind(&self) -> usize {
        self.kind_val
    }
    fn increment_kind(&mut self) {
        self.kind_val += 1;
    }

    fn format(
        &mut self,
        w: &mut LineWriter,
        depth: usize,
        spans: &mut [Box<dyn TextSpan>],
        options: &FmtOptions,
        parent: &dyn Rule,
    ) -> bool {
        if spans.len() == 1 {
            return spans[0].format(w, depth, options, parent);
        }

        let mut newline = false;
        for span in spans {
            newline = span.format(w, depth, options, self);
        }
        newline
    }
}

#[derive(Debug)]
struct ArgsRule {
    kind_val: usize,
}

impl ArgsRule {
    fn new(kind: usize) -> Self {
        Self { kind_val: kind }
    }
}

impl Rule for ArgsRule {
    fn kind(&self) -> usize {
        self.kind_val
    }
    fn increment_kind(&mut self) {
        if self.kind_val < 1 {
            self.kind_val += 1;
        }
    }
    fn nb_kinds(&self) -> usize {
        2
    }

    fn first_line_length(
        &self,
        spans: &[Box<dyn TextSpan>],
        _options: &FmtOptions,
        _parent: &dyn Rule,
    ) -> usize {
        match self.kind_val {
            0 => {
                let mut len = 0;
                for (i, span) in spans.iter().enumerate() {
                    let is_last = i == spans.len() - 1;
                    len += span.length();
                    if !is_last {
                        len += 1; // for the space
                    }
                }
                len
            }
            1 => 0,
            _ => 0,
        }
    }

    fn format(
        &mut self,
        w: &mut LineWriter,
        depth: usize,
        spans: &mut [Box<dyn TextSpan>],
        options: &FmtOptions,
        _parent: &dyn Rule,
    ) -> bool {
        match self.kind_val {
            0 => self.format_spaced(w, depth, spans, options),
            1 => self.format_multiline(w, depth, spans, options),
            _ => false,
        }
    }
}

impl ArgsRule {
    fn format_spaced(
        &mut self,
        w: &mut LineWriter,
        depth: usize,
        spans: &mut [Box<dyn TextSpan>],
        options: &FmtOptions,
    ) -> bool {
        let mut newline = false;
        let spans_len = spans.len();
        for (i, span) in spans.iter_mut().enumerate() {
            let is_last = i == spans_len - 1;
            newline = span.format(w, depth, options, self);
            if !is_last {
                w.write(" ");
            }
        }
        newline
    }

    fn format_multiline(
        &mut self,
        w: &mut LineWriter,
        depth: usize,
        spans: &mut [Box<dyn TextSpan>],
        options: &FmtOptions,
    ) -> bool {
        if !w.is_empty_line() {
            w.newline();
        }
        w.indent(depth);

        let mut newline = false;
        let spans_len = spans.len();
        for (i, span) in spans.iter_mut().enumerate() {
            let is_last = i == spans_len - 1;
            newline = span.format(w, if is_last { depth - 1 } else { depth }, options, self);
            if !newline && !is_last {
                w.newline();
                w.indent(depth);
            }
        }

        if !newline {
            w.newline();
        }
        w.indent(depth - 1);
        true
    }
}

#[derive(Debug)]
struct FieldsRule {
    kind_val: usize,
}

impl FieldsRule {
    fn new(kind: usize) -> Self {
        Self { kind_val: kind }
    }
}

impl Rule for FieldsRule {
    fn kind(&self) -> usize {
        self.kind_val
    }
    fn increment_kind(&mut self) {
        if self.kind_val < 1 {
            self.kind_val += 1;
        }
    }
    fn nb_kinds(&self) -> usize {
        2
    }

    fn first_line_length(
        &self,
        spans: &[Box<dyn TextSpan>],
        _options: &FmtOptions,
        _parent: &dyn Rule,
    ) -> usize {
        if spans.is_empty() {
            return 0;
        }

        let mut len = 1; // first space
        for (i, span) in spans.iter().enumerate() {
            let is_last = i == spans.len() - 1;
            len += span.length();
            if !is_last {
                len += 1; // for the space
            }
        }
        len + 1 // last space
    }

    fn format(
        &mut self,
        w: &mut LineWriter,
        depth: usize,
        spans: &mut [Box<dyn TextSpan>],
        options: &FmtOptions,
        _parent: &dyn Rule,
    ) -> bool {
        match self.kind_val {
            0 => self.format_spaced(w, depth, spans, options),
            1 => self.format_multiline(w, depth, spans, options),
            _ => false,
        }
    }
}

impl FieldsRule {
    fn format_spaced(
        &mut self,
        w: &mut LineWriter,
        depth: usize,
        spans: &mut [Box<dyn TextSpan>],
        options: &FmtOptions,
    ) -> bool {
        if spans.is_empty() {
            return false;
        }

        let mut newline = false;
        w.write(" ");
        let spans_len = spans.len();
        for (i, span) in spans.iter_mut().enumerate() {
            let is_last = i == spans_len - 1;
            newline = span.format(w, depth, options, self);
            if !is_last {
                w.write(" ");
            }
        }
        w.write(" ");
        newline
    }

    fn format_multiline(
        &mut self,
        w: &mut LineWriter,
        depth: usize,
        spans: &mut [Box<dyn TextSpan>],
        options: &FmtOptions,
    ) -> bool {
        w.newline();
        w.indent(depth);

        let mut newline = false;
        let spans_len = spans.len();
        for (i, span) in spans.iter_mut().enumerate() {
            let is_last = i == spans_len - 1;
            newline = span.format(w, if is_last { depth - 1 } else { depth }, options, self);
            if !newline && !is_last {
                w.newline();
                w.indent(depth);
            }
        }

        if !newline {
            w.newline();
        }
        w.indent(depth - 1);
        true
    }
}

#[derive(Debug)]
struct StmtsRule {
    kind_val: usize,
}

impl StmtsRule {
    fn new() -> Self {
        Self { kind_val: 0 }
    }

    fn is_empty(&self, spans: &[Box<dyn TextSpan>]) -> bool {
        spans.iter().all(|span| span.is_empty())
    }
}

impl Rule for StmtsRule {
    fn kind(&self) -> usize {
        self.kind_val
    }
    fn increment_kind(&mut self) {
        self.kind_val += 1;
    }

    fn first_line_length(
        &self,
        _spans: &[Box<dyn TextSpan>],
        _options: &FmtOptions,
        _parent: &dyn Rule,
    ) -> usize {
        0
    }

    fn format(
        &mut self,
        w: &mut LineWriter,
        depth: usize,
        spans: &mut [Box<dyn TextSpan>],
        options: &FmtOptions,
        _parent: &dyn Rule,
    ) -> bool {
        if self.is_empty(spans) {
            return false;
        }

        if !w.is_empty_line() {
            w.newline();
        }
        w.indent(depth);

        let mut newline = false;
        let spans_len = spans.len();
        for (i, span) in spans.iter_mut().enumerate() {
            let is_last = i == spans_len - 1;
            if !span.is_empty() {
                newline = span.format(w, if is_last { depth - 1 } else { depth }, options, self);
                if !is_last && !newline {
                    w.newline();
                    w.indent(depth);
                }
            }
        }

        if !newline && !w.start_of_line() {
            w.newline();
            // newline = true;
        }
        w.indent(depth - 1);
        true
    }
}

#[derive(Debug)]
struct HardSplitRule {
    kind_val: usize,
}

impl HardSplitRule {
    fn new() -> Self {
        Self { kind_val: 0 }
    }
}

impl Rule for HardSplitRule {
    fn kind(&self) -> usize {
        self.kind_val
    }
    fn increment_kind(&mut self) {
        self.kind_val += 1;
    }

    fn first_line_length(
        &self,
        spans: &[Box<dyn TextSpan>],
        _options: &FmtOptions,
        _parent: &dyn Rule,
    ) -> usize {
        let mut len = 0;
        for span in spans {
            len += span.length();
        }
        len
    }

    fn format(
        &mut self,
        w: &mut LineWriter,
        depth: usize,
        spans: &mut [Box<dyn TextSpan>],
        options: &FmtOptions,
        _parent: &dyn Rule,
    ) -> bool {
        if w.is_empty_line() {
            w.indent(depth);
        }

        let mut newline = false;
        for span in spans {
            newline = span.format(w, depth, options, self);
        }

        if !newline {
            w.newline();
            w.indent(depth);
        }
        true
    }
}

#[derive(Debug)]
struct SimpleRule {
    kind_val: usize,
}

impl SimpleRule {
    fn new() -> Self {
        Self { kind_val: 0 }
    }
}

impl Rule for SimpleRule {
    fn kind(&self) -> usize {
        self.kind_val
    }
    fn increment_kind(&mut self) {
        if self.kind_val < 1 {
            self.kind_val += 1;
        }
    }
    fn nb_kinds(&self) -> usize {
        2
    }

    fn first_line_length(
        &self,
        spans: &[Box<dyn TextSpan>],
        _options: &FmtOptions,
        _parent: &dyn Rule,
    ) -> usize {
        self.compute_length(0, spans)
    }

    fn format(
        &mut self,
        w: &mut LineWriter,
        depth: usize,
        spans: &mut [Box<dyn TextSpan>],
        options: &FmtOptions,
        _parent: &dyn Rule,
    ) -> bool {
        if w.is_empty_line() {
            w.indent(depth);
        }

        match self.kind_val {
            0 => self.format_no_split(0, w, depth, spans, options),
            1 => self.format_split(0, w, depth, spans, options),
            _ => false,
        }
    }
}

impl SimpleRule {
    fn format_no_split(
        &mut self,
        start: usize,
        w: &mut LineWriter,
        depth: usize,
        spans: &mut [Box<dyn TextSpan>],
        options: &FmtOptions,
    ) -> bool {
        let mut newline = false;
        for span in &mut spans[start..] {
            newline = span.format(w, depth, options, self);
            if newline {
                w.indent(depth);
            }
        }
        newline
    }

    fn format_split(
        &mut self,
        start: usize,
        w: &mut LineWriter,
        depth: usize,
        spans: &mut [Box<dyn TextSpan>],
        options: &FmtOptions,
    ) -> bool {
        let mut newline = false;
        let spans_len = spans.len();
        for (i, span) in spans[start..].iter_mut().enumerate() {
            let actual_i = start + i;
            let is_last = actual_i == spans_len - 1;

            newline = span.format(w, depth, options, self);

            if !is_last && !newline {
                w.newline();
                w.indent(depth);
                newline = true;
            }
        }
        newline
    }

    fn compute_length(&self, start: usize, spans: &[Box<dyn TextSpan>]) -> usize {
        spans[start..].iter().map(|span| span.length()).sum()
    }
}

#[derive(Debug)]
struct LineBeforeRule {
    kind_val: usize,
}

impl LineBeforeRule {
    fn new() -> Self {
        Self { kind_val: 0 }
    }
}

impl Rule for LineBeforeRule {
    fn kind(&self) -> usize {
        self.kind_val
    }
    fn increment_kind(&mut self) {
        self.kind_val += 1;
    }

    fn first_line_length(
        &self,
        _spans: &[Box<dyn TextSpan>],
        _options: &FmtOptions,
        _parent: &dyn Rule,
    ) -> usize {
        0
    }

    fn format(
        &mut self,
        w: &mut LineWriter,
        depth: usize,
        spans: &mut [Box<dyn TextSpan>],
        options: &FmtOptions,
        _parent: &dyn Rule,
    ) -> bool {
        if spans.is_empty() {
            return false;
        }

        if !w.is_newline() {
            w.newline();
            if !w.is_newline() {
                w.newline();
            }
        }

        let mut default_rule = DefaultRule::new();
        default_rule.format(w, depth, spans, options, self)
    }
}

#[derive(Debug)]
struct SpaceAfterRule {
    kind_val: usize,
}

impl SpaceAfterRule {
    fn new() -> Self {
        Self { kind_val: 0 }
    }
}

impl Rule for SpaceAfterRule {
    fn kind(&self) -> usize {
        self.kind_val
    }
    fn increment_kind(&mut self) {
        self.kind_val += 1;
    }

    fn format(
        &mut self,
        w: &mut LineWriter,
        depth: usize,
        spans: &mut [Box<dyn TextSpan>],
        options: &FmtOptions,
        _parent: &dyn Rule,
    ) -> bool {
        let mut default_rule = DefaultRule::new();
        default_rule.format(w, depth, spans, options, self);
        w.write(" ");
        false
    }
}

#[derive(Debug)]
struct BinOpRule {
    kind_val: usize,
    weight: u8,
}

impl BinOpRule {
    fn new() -> Self {
        Self {
            kind_val: 0,
            weight: 0,
        }
    }

    fn set_weight(&mut self, weight: u8) {
        self.weight = weight;
    }
}

impl Rule for BinOpRule {
    fn kind(&self) -> usize {
        self.kind_val
    }
    fn increment_kind(&mut self) {
        if self.kind_val < 1 {
            self.kind_val += 1;
        }
    }
    fn nb_kinds(&self) -> usize {
        2
    }

    fn first_line_length(
        &self,
        spans: &[Box<dyn TextSpan>],
        _options: &FmtOptions,
        _parent: &dyn Rule,
    ) -> usize {
        match self.kind_val {
            0 => {
                if spans.is_empty() {
                    return 0;
                }

                let mut len = 0;
                for (i, span) in spans.iter().enumerate() {
                    len += span.length();
                    let is_last = i == spans.len() - 1;
                    if !is_last {
                        len += 1; // for the space
                    }
                    if i == 0 {
                        len += 1; // for the space after first element
                    }
                }
                len
            }
            1 => 0,
            _ => 0,
        }
    }

    fn format(
        &mut self,
        w: &mut LineWriter,
        depth: usize,
        spans: &mut [Box<dyn TextSpan>],
        options: &FmtOptions,
        _parent: &dyn Rule,
    ) -> bool {
        match self.kind_val {
            0 => self.format_inline(w, depth, spans, options),
            1 => self.format_multiline(w, depth, spans, options),
            _ => false,
        }
    }
}

impl BinOpRule {
    fn format_inline(
        &mut self,
        w: &mut LineWriter,
        depth: usize,
        spans: &mut [Box<dyn TextSpan>],
        options: &FmtOptions,
    ) -> bool {
        if spans.is_empty() {
            return false;
        }

        let mut newline = false;
        let spans_len = spans.len();
        for (i, span) in spans.iter_mut().enumerate() {
            newline = span.format(w, depth, options, self);
            let is_last = i == spans_len - 1;
            if !is_last {
                w.write(" ");
            }
            if i == 0 {
                w.write(" ");
            }
        }
        newline
    }

    fn format_multiline(
        &mut self,
        w: &mut LineWriter,
        depth: usize,
        spans: &mut [Box<dyn TextSpan>],
        options: &FmtOptions,
    ) -> bool {
        if spans.is_empty() {
            return false;
        }

        let mut newline = false;
        if let Some(first) = spans.get_mut(0) {
            newline = first.format(w, depth + 1, options, self);
        }

        if !newline {
            w.newline();
            w.indent(depth + 1);
        }

        for span in &mut spans[1..] {
            newline = span.format(w, depth + 1, options, self);
        }
        newline
    }
}

#[derive(Debug)]
struct RootRule {
    kind_val: usize,
}

impl RootRule {
    fn new() -> Self {
        Self { kind_val: 0 }
    }
}

impl Rule for RootRule {
    fn kind(&self) -> usize {
        self.kind_val
    }
    fn increment_kind(&mut self) {
        self.kind_val += 1;
    }

    fn format(
        &mut self,
        w: &mut LineWriter,
        depth: usize,
        spans: &mut [Box<dyn TextSpan>],
        options: &FmtOptions,
        _parent: &dyn Rule,
    ) -> bool {
        let mut newline = false;
        let spans_len = spans.len();
        for (i, span) in spans.iter_mut().enumerate() {
            newline = span.format(w, depth, options, self);
            if !newline && i < spans_len - 1 {
                w.newline();
                w.indent(depth);
                newline = true;
            }
        }
        newline
    }
}

#[derive(Debug)]
struct NoSplitRule {
    kind_val: usize,
}

impl NoSplitRule {
    fn new() -> Self {
        Self { kind_val: 0 }
    }
}

impl Rule for NoSplitRule {
    fn kind(&self) -> usize {
        self.kind_val
    }
    fn increment_kind(&mut self) {
        self.kind_val += 1;
    }

    fn first_line_length(
        &self,
        _spans: &[Box<dyn TextSpan>],
        _options: &FmtOptions,
        _parent: &dyn Rule,
    ) -> usize {
        0 // never split
    }

    fn format(
        &mut self,
        w: &mut LineWriter,
        depth: usize,
        spans: &mut [Box<dyn TextSpan>],
        options: &FmtOptions,
        _parent: &dyn Rule,
    ) -> bool {
        let mut newline = false;
        for span in spans {
            newline = span.format(w, depth, options, self);
        }
        newline
    }
}

#[derive(Debug)]
struct LineWriter {
    lines: Vec<String>,
    indent_str: String,
}

impl LineWriter {
    fn new(options: &FmtOptions) -> Self {
        Self {
            lines: vec![String::new()],
            indent_str: " ".repeat(options.indent),
        }
    }

    fn write(&mut self, text: &str) {
        if let Some(last) = self.lines.last_mut() {
            last.push_str(text);
        }
    }

    fn newline(&mut self) {
        // Clean up trailing whitespace
        if let Some(last) = self.lines.last_mut() {
            if last.trim().is_empty() {
                last.clear();
            }
        }
        self.lines.push(String::new());
    }

    fn is_empty_line(&self) -> bool {
        self.lines
            .last()
            .map_or(true, |line| line.trim().is_empty())
    }

    fn is_newline(&self) -> bool {
        if self.lines.len() <= 1 {
            return self.lines.first().map_or(true, |line| line.is_empty());
        }

        let len = self.lines.len();
        len >= 2 && self.lines[len - 2].is_empty() && self.lines[len - 1].is_empty()
    }

    fn start_of_line(&self) -> bool {
        self.lines.last().map_or(true, |line| line.is_empty())
    }

    fn indent(&mut self, depth: usize) {
        if let Some(last) = self.lines.last_mut() {
            *last = self.indent_str.repeat(depth.max(0));
        }
    }

    fn line_length(&self) -> usize {
        self.lines.last().map_or(0, |line| line.len())
    }

    fn finalize(mut self, options: &FmtOptions) -> String {
        if options.eol_last {
            if !self.is_empty_line() {
                self.lines.push(String::new());
            }
        } else if self.is_empty_line() {
            self.lines.pop();
        }

        self.lines.join("\n")
    }
}

#[derive(Debug)]
pub struct Formatter {
    curr: Group,
    stack: Vec<Group>,
    depth: usize,
}

impl Formatter {
    fn new() -> Self {
        Self {
            curr: Group::new(Box::new(RootRule::new()), 0),
            stack: Vec::new(),
            depth: 0,
        }
    }

    fn write(&mut self, text: String, force_newline: bool) -> &mut Self {
        self.curr.write(text, force_newline);
        self
    }

    fn newline(&mut self) {
        let group = Group::new(Box::new(HardSplitRule::new()), self.depth);
        self.curr.push(Box::new(group));
    }

    fn new_chunk(&mut self) -> &mut Self {
        self.curr.new_chunk();
        self
    }

    fn indent(&mut self, rule: Box<dyn Rule>) {
        self.depth += 1;
        self.new_group(rule);
    }

    fn outdent(&mut self) {
        self.close_group();
        self.depth = self.depth.saturating_sub(1);
    }

    fn open_group(&mut self, group: Group) {
        let curr = std::mem::replace(&mut self.curr, group);
        self.stack.push(curr);
    }

    fn new_group(&mut self, rule: Box<dyn Rule>) {
        let group = Group::new(rule, self.depth);
        self.open_group(group);
    }

    fn close_group(&mut self) {
        if let Some(prev) = self.stack.pop() {
            self.curr.close();
            let curr = std::mem::replace(&mut self.curr, prev);
            self.curr.push(Box::new(curr));
        }
    }

    fn token(&mut self, token: &Token, source: &str) {
        match token.kind {
            TokenKind::NewLine(_) | TokenKind::Space(_) => {
                // Handle these manually while parsing
            }
            TokenKind::EolComment => {
                let image = &source[token.span.as_range()];
                self.write(format!(" {}", image), true);
                self.new_chunk();
            }
            TokenKind::BlockComment => {
                let image = &source[token.span.as_range()];
                self.write(image.to_string(), false);
            }
            TokenKind::DocComment => {
                let image = &source[token.span.as_range()];
                self.write(image.to_string(), false);
                self.new_chunk();
            }
            TokenKind::Colon => {
                self.write(": ".to_string(), false);
            }
            _ => {
                let image = &source[token.span.as_range()];
                if self.is_keyword(token) {
                    self.write(format!("{} ", image), false);
                } else {
                    self.write(image.to_string(), false);
                }
            }
        }
    }

    fn is_keyword(&self, token: &Token) -> bool {
        matches!(
            token.kind,
            TokenKind::Abstract
                | TokenKind::As
                | TokenKind::At
                | TokenKind::Break
                | TokenKind::Breakpoint
                | TokenKind::Catch
                | TokenKind::Continue
                | TokenKind::Do
                | TokenKind::Else
                | TokenKind::Enum
                | TokenKind::Extends
                | TokenKind::False
                | TokenKind::For
                | TokenKind::Fn
                | TokenKind::If
                | TokenKind::In
                | TokenKind::Is
                | TokenKind::Limit
                | TokenKind::Native
                | TokenKind::Null
                | TokenKind::NaN
                | TokenKind::Infinity
                | TokenKind::Private
                | TokenKind::Return
                | TokenKind::Sampling
                | TokenKind::Skip
                | TokenKind::Static
                | TokenKind::Task
                | TokenKind::This
                | TokenKind::Throw
                | TokenKind::Try
                | TokenKind::Type
                | TokenKind::True
                | TokenKind::TypeOf
                | TokenKind::Use
                | TokenKind::Var
                | TokenKind::While
                | TokenKind::Without
        )
    }

    fn format(mut self, options: &FmtOptions) -> String {
        let mut w = LineWriter::new(options);
        let default_rule = DefaultRule::new();
        self.curr.format(&mut w, 0, options, &default_rule);
        w.finalize(options)
    }
}

// Helper functions for token lookups
fn lookforward<T>(nodes: &[T], start: usize, predicate: impl Fn(&T) -> bool) -> usize {
    let mut found = false;
    let mut i = start + 1;

    while i < nodes.len() {
        if predicate(&nodes[i]) {
            found = true;
        } else if found {
            return i;
        }
        i += 1;
    }

    if found { i } else { start }
}

fn lookbackward<T>(nodes: &[T], start: usize, predicate: impl Fn(&T) -> bool) -> usize {
    let mut found = false;
    let mut i = start;

    while i > 0 {
        i -= 1;
        if predicate(&nodes[i]) {
            found = true;
        } else if found {
            return i + 1;
        }
    }

    if found { 0 } else { start }
}

fn has_token_kind(node: &Node, kind: TokenKind) -> bool {
    for child in &node.children {
        if let CstNode::Token(token) = child {
            if token.kind == kind {
                return true;
            }
        }
    }
    false
}

fn has_field(node: &Node, field_name: &str) -> bool {
    for child in &node.children {
        if let CstNode::Node(child_node) = child {
            if child_node.field_name.as_deref() == Some(field_name) {
                return true;
            }
        }
    }
    false
}

// Extension trait for Node to add helper methods
trait NodeExt {
    fn get_token_by_kind(&self, kind: TokenKind) -> Option<&Token>;
}

impl NodeExt for Node {
    fn get_token_by_kind(&self, kind: TokenKind) -> Option<&Token> {
        for child in &self.children {
            if let CstNode::Token(token) = child {
                if token.kind == kind {
                    return Some(token);
                }
            }
        }
        None
    }
}

// Main formatting function
pub fn fmt(
    _source: &str,
    options: Option<FmtOptions>,
) -> Result<String, Box<dyn std::error::Error>> {
    let options = options.unwrap_or_default();

    // For a complete implementation, you would:
    // 1. Parse the source code into a CST
    // 2. Create a FormatterVisitor with the source
    // 3. Visit all nodes in the CST
    // 4. Return the formatted result

    // This is a simplified placeholder that would need integration with your parser
    let mut formatter = Formatter::new();

    // Example: Format a simple piece of code
    formatter.write("// Example formatting".to_string(), false);
    formatter.newline();
    formatter.write("fn main() {".to_string(), false);
    formatter.indent(Box::new(StmtsRule::new()));
    formatter.write("println!(\"Hello, world!\");".to_string(), false);
    formatter.outdent();
    formatter.write("}".to_string(), false);

    Ok(formatter.format(&options))
}

// Visitor pattern for CST nodes
trait CstVisitor {
    fn visit_node(&mut self, node: &Node, source: &str);
    fn visit_token(&mut self, token: &Token, source: &str);
}

struct FormatterVisitor<'a> {
    formatter: &'a mut Formatter,
    source: &'a str,
}

impl<'a> FormatterVisitor<'a> {
    fn new(formatter: &'a mut Formatter, source: &'a str) -> Self {
        Self { formatter, source }
    }

    fn visit(&mut self, node: &CstNode) {
        match node {
            CstNode::Node(n) => self.visit_node(n, self.source),
            CstNode::Token(t) => self.visit_token(t, self.source),
            CstNode::Error(_) => {
                // Skip errors during formatting
            }
        }
    }
}

impl<'a> CstVisitor for FormatterVisitor<'a> {
    fn visit_node(&mut self, node: &Node, source: &str) {
        match node.kind {
            NodeKind::Module => {
                let mut rule: Box<dyn Rule> = Box::new(HardSplitRule::new());
                for child in &node.children {
                    if let CstNode::Node(child_node) = child {
                        match child_node.field_name.as_deref() {
                            Some("stmt") => match child_node.kind {
                                NodeKind::ModPragma => {
                                    self.visit(child);
                                }
                                NodeKind::ModVarDecl => {
                                    self.formatter.new_group(Box::new(HardSplitRule::new()));
                                    self.visit(child);
                                    self.formatter.close_group();
                                }
                                _ => {
                                    self.formatter.new_group(rule);
                                    self.visit(child);
                                    self.formatter.close_group();
                                    rule = Box::new(LineBeforeRule::new());
                                }
                            },
                            _ => {
                                self.visit(child);
                            }
                        }
                    } else if let CstNode::Token(token) = child {
                        match token.kind {
                            TokenKind::BlockComment | TokenKind::EolComment => {
                                self.formatter.new_group(Box::new(HardSplitRule::new()));
                                let image = &source[token.span.as_range()];
                                self.formatter.write(image.to_string(), false);
                                self.formatter.close_group();
                                rule = Box::new(LineBeforeRule::new());
                            }
                            TokenKind::NewLine(_) => {
                                // Handle newlines appropriately
                                self.formatter.newline();
                            }
                            _ => {
                                self.formatter.token(token, source);
                            }
                        }
                    }
                }
            }

            NodeKind::FnParams => {
                for child in &node.children {
                    if let CstNode::Token(token) = child {
                        match token.kind {
                            TokenKind::OpenParen => {
                                self.formatter.write("(".to_string(), false);
                                self.formatter.indent(Box::new(ArgsRule::new(0)));
                            }
                            TokenKind::CloseParen => {
                                self.formatter.outdent();
                                self.formatter.write(")".to_string(), false);
                            }
                            TokenKind::Comma => {
                                self.formatter.write(",".to_string(), false);
                                self.formatter.new_chunk();
                            }
                            _ => {
                                self.formatter.token(token, source);
                            }
                        }
                    } else {
                        self.visit(child);
                    }
                }
            }

            NodeKind::Body => {
                let mut open = false;
                let has_stmts = has_field(node, "stmt");

                for child in &node.children {
                    if let CstNode::Token(token) = child {
                        match token.kind {
                            TokenKind::OpenCurly => {
                                self.formatter.write(" {".to_string(), false);
                                self.formatter.indent(Box::new(StmtsRule::new()));
                                open = true;
                            }
                            TokenKind::CloseCurly => {
                                if open {
                                    self.formatter.outdent();
                                }
                                self.formatter.write("}".to_string(), false);
                            }
                            TokenKind::NewLine(_) => {
                                if has_stmts {
                                    self.formatter.newline();
                                }
                            }
                            _ => {
                                self.formatter.token(token, source);
                            }
                        }
                    } else if let CstNode::Node(child_node) = child {
                        if child_node.field_name.as_deref() == Some("stmt") {
                            self.formatter.new_group(Box::new(DefaultRule::new()));
                            self.visit(child);
                            self.formatter.close_group();
                        } else {
                            self.visit(child);
                        }
                    }
                }
            }

            NodeKind::CallArgs => {
                let has_args = has_field(node, "arg");
                let has_newlines = has_token_kind(node, TokenKind::NewLine(1));
                let kind = if has_args && has_newlines { 1 } else { 0 };

                for child in &node.children {
                    if let CstNode::Token(token) = child {
                        match token.kind {
                            TokenKind::OpenParen => {
                                self.formatter.write("(".to_string(), false);
                                self.formatter.indent(Box::new(ArgsRule::new(kind)));
                            }
                            TokenKind::CloseParen => {
                                self.formatter.outdent();
                                self.formatter.write(")".to_string(), false);
                            }
                            TokenKind::Comma => {
                                let image = &source[token.span.as_range()];
                                self.formatter.write(image.to_string(), false);
                                self.formatter.new_chunk();
                            }
                            _ => {
                                self.formatter.token(token, source);
                            }
                        }
                    } else {
                        self.visit(child);
                    }
                }
            }

            NodeKind::BinaryExpr => {
                let rule = BinOpRule::new();
                self.formatter.new_group(Box::new(rule));

                for child in &node.children {
                    if let CstNode::Node(child_node) = child {
                        match child_node.field_name.as_deref() {
                            Some("lhs") => {
                                self.formatter.new_group(Box::new(DefaultRule::new()));
                                self.visit(child);
                                self.formatter.close_group();
                            }
                            Some("op") => {
                                // Handle operator formatting
                                self.visit(child);
                                self.formatter.new_chunk();
                            }
                            Some("rhs") => {
                                self.formatter.new_group(Box::new(DefaultRule::new()));
                                self.visit(child);
                                self.formatter.close_group();
                            }
                            _ => {
                                self.formatter.new_group(Box::new(DefaultRule::new()));
                                self.visit(child);
                                self.formatter.close_group();
                            }
                        }
                    } else {
                        self.visit(child);
                    }
                }

                self.formatter.close_group();
            }

            NodeKind::IfStmt => {
                for child in &node.children {
                    if let CstNode::Token(token) = child {
                        match token.kind {
                            TokenKind::If => {
                                self.formatter.write("if ".to_string(), false);
                            }
                            TokenKind::Else => {
                                self.formatter.write(" else".to_string(), false);
                            }
                            _ => {
                                self.formatter.token(token, source);
                            }
                        }
                    } else if let CstNode::Node(child_node) = child {
                        match child_node.field_name.as_deref() {
                            Some("condition") => {
                                self.formatter.new_group(Box::new(DefaultRule::new()));
                                self.visit(child);
                                self.formatter.close_group();
                            }
                            Some("then_body") | Some("else_body") => {
                                self.visit(child);
                            }
                            _ => {
                                self.visit(child);
                            }
                        }
                    }
                }
            }

            NodeKind::ForInStmt => {
                for child in &node.children {
                    if let CstNode::Token(token) = child {
                        match token.kind {
                            TokenKind::For => {
                                self.formatter.write("for ".to_string(), false);
                            }
                            TokenKind::In => {
                                self.formatter.write(" in ".to_string(), false);
                            }
                            _ => {
                                self.formatter.token(token, source);
                            }
                        }
                    } else {
                        self.visit(child);
                    }
                }
            }

            NodeKind::FnDecl => {
                for child in &node.children {
                    if let CstNode::Token(token) = child {
                        match token.kind {
                            TokenKind::Fn => {
                                self.formatter.write("fn ".to_string(), false);
                            }
                            _ => {
                                self.formatter.token(token, source);
                            }
                        }
                    } else if let CstNode::Node(child_node) = child {
                        match child_node.field_name.as_deref() {
                            Some("name") => {
                                self.visit(child);
                            }
                            Some("params") => {
                                self.visit(child);
                            }
                            Some("body") => {
                                self.visit(child);
                            }
                            _ => {
                                self.visit(child);
                            }
                        }
                    }
                }
            }

            NodeKind::VarDecl => {
                for child in &node.children {
                    if let CstNode::Token(token) = child {
                        match token.kind {
                            TokenKind::Var => {
                                self.formatter.write("var ".to_string(), false);
                            }
                            TokenKind::Eq => {
                                self.formatter.write(" = ".to_string(), false);
                            }
                            _ => {
                                self.formatter.token(token, source);
                            }
                        }
                    } else {
                        self.visit(child);
                    }
                }
            }

            NodeKind::ArrayInlineExpr => {
                let has_elements = has_field(node, "element");
                let has_newlines = has_token_kind(node, TokenKind::NewLine(1));
                let kind = if has_elements && has_newlines { 1 } else { 0 };

                for child in &node.children {
                    if let CstNode::Token(token) = child {
                        match token.kind {
                            TokenKind::OpenSquare => {
                                self.formatter.write("[".to_string(), false);
                                self.formatter.indent(Box::new(ArgsRule::new(kind)));
                            }
                            TokenKind::CloseSquare => {
                                self.formatter.outdent();
                                self.formatter.write("]".to_string(), false);
                            }
                            TokenKind::Comma => {
                                self.formatter.write(",".to_string(), false);
                                self.formatter.new_chunk();
                            }
                            _ => {
                                self.formatter.token(token, source);
                            }
                        }
                    } else {
                        self.visit(child);
                    }
                }
            }

            NodeKind::ObjectExpr => {
                let has_fields = has_field(node, "field");
                let has_newlines = has_token_kind(node, TokenKind::NewLine(1));
                let kind = if has_fields && has_newlines { 1 } else { 0 };

                for child in &node.children {
                    if let CstNode::Token(token) = child {
                        match token.kind {
                            TokenKind::OpenCurly => {
                                self.formatter.write("{".to_string(), false);
                                self.formatter.indent(Box::new(FieldsRule::new(kind)));
                            }
                            TokenKind::CloseCurly => {
                                self.formatter.outdent();
                                self.formatter.write("}".to_string(), false);
                            }
                            TokenKind::Comma => {
                                self.formatter.write(",".to_string(), false);
                                self.formatter.new_chunk();
                            }
                            _ => {
                                self.formatter.token(token, source);
                            }
                        }
                    } else {
                        self.visit(child);
                    }
                }
            }

            // Add more node types as needed
            _ => {
                // Default case: visit all children
                for child in &node.children {
                    self.visit(child);
                }
            }
        }
    }

    fn visit_token(&mut self, token: &Token, source: &str) {
        self.formatter.token(token, source);
    }
}

// Main formatting function that integrates with a parser
pub fn format_cst(
    root: &Node,
    source: &str,
    options: Option<FmtOptions>,
) -> Result<String, Box<dyn std::error::Error>> {
    let options = options.unwrap_or_default();
    let mut formatter = Formatter::new();
    let mut visitor = FormatterVisitor::new(&mut formatter, &source);

    visitor.visit_node(root, source);

    Ok(formatter.format(&options))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_formatter_creation() {
        let formatter = Formatter::new();
        let options = FmtOptions::default();
        let result = formatter.format(&options);
        assert_eq!(result, "");
    }

    #[test]
    fn test_line_writer() {
        let options = FmtOptions::default();
        let mut writer = LineWriter::new(&options);

        writer.write("hello");
        writer.write(" world");
        writer.newline();
        writer.indent(1);
        writer.write("indented");

        let result = writer.finalize(&options);
        assert_eq!(result, "hello world\n  indented");
    }

    #[test]
    fn test_basic_formatting() {
        let options = FmtOptions::default();
        let result = fmt("// test", Some(options)).unwrap();
        assert!(result.contains("Example formatting"));
    }

    #[test]
    fn test_chunk_operations() {
        let mut chunk = Chunk::new("test".to_string(), false);
        assert!(!chunk.is_empty());
        assert_eq!(chunk.length(), 4);

        chunk.append(" more");
        assert_eq!(chunk.length(), 9);
    }

    #[test]
    fn test_group_operations() {
        let rule = Box::new(DefaultRule::new());
        let mut group = Group::new(rule, 0);

        assert!(group.is_empty());
        group.write("test".to_string(), false);
        assert!(!group.is_empty());
        assert_eq!(group.length(), 4);
    }

    #[test]
    fn test_args_rule() {
        let mut rule = ArgsRule::new(0);
        assert_eq!(rule.kind(), 0);
        assert_eq!(rule.nb_kinds(), 2);

        rule.increment_kind();
        assert_eq!(rule.kind(), 1);

        rule.increment_kind(); // Should not increment beyond max
        assert_eq!(rule.kind(), 1);
    }

    #[test]
    fn test_line_writer_operations() {
        let options = FmtOptions::default();
        let mut writer = LineWriter::new(&options);

        assert!(writer.is_empty_line());
        assert!(writer.start_of_line());

        writer.write("test");
        assert!(!writer.is_empty_line());
        assert!(!writer.start_of_line());
        assert_eq!(writer.line_length(), 4);

        writer.newline();
        assert!(writer.is_empty_line());
        assert!(writer.start_of_line());
        assert_eq!(writer.line_length(), 0);
    }
}
