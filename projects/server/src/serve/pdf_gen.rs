use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::path::PathBuf;
use typst::diag::{FileError, FileResult};
use typst::foundations::{Bytes, Datetime};
use typst::layout::PagedDocument;
use typst::syntax::{FileId, Source};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};

// ── In-memory World ───────────────────────────────────────────────────────────

struct MemWorld {
    source: Source,
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    fonts: Vec<Font>,
}

impl MemWorld {
    fn new(content: String) -> Self {
        let mut book = FontBook::new();
        let mut fonts = Vec::new();

        for data in typst_assets::fonts() {
            let bytes = Bytes::new(data);
            for font in Font::iter(bytes) {
                book.push(font.info().clone());
                fonts.push(font);
            }
        }

        Self {
            source: Source::detached(content),
            library: LazyHash::new(Library::builder().build()),
            book: LazyHash::new(book),
            fonts,
        }
    }
}

impl World for MemWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }
    fn book(&self) -> &LazyHash<FontBook> {
        &self.book
    }
    fn main(&self) -> FileId {
        self.source.id()
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.source.id() {
            Ok(self.source.clone())
        } else {
            Err(FileError::NotFound(PathBuf::from(
                id.vpath().as_rootless_path(),
            )))
        }
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        Err(FileError::NotFound(PathBuf::from(
            id.vpath().as_rootless_path(),
        )))
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.get(index).cloned()
    }

    fn today(&self, _offset: Option<i64>) -> Option<Datetime> {
        None
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Render a slice of (title, markdown_content) pairs into a single PDF.
pub fn render_pdf(pages: &[(String, String)]) -> Result<Vec<u8>, String> {
    let source = pages_to_typst(pages);
    let world = MemWorld::new(source);

    let doc: PagedDocument = typst::compile(&world).output.map_err(|errs| {
        errs.iter()
            .map(|e| e.message.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    })?;

    typst_pdf::pdf(&doc, &typst_pdf::PdfOptions::default()).map_err(|errs| {
        errs.iter()
            .map(|e| e.message.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    })
}

// ── Typst document builder ────────────────────────────────────────────────────

fn pages_to_typst(pages: &[(String, String)]) -> String {
    let mut out = String::from(
        "#set text(size: 11pt)\n\
         #set page(margin: (x: 2.5cm, y: 2cm))\n\
         #set heading(numbering: none)\n\
         #show raw: set text(size: 9pt)\n\n",
    );

    for (i, (_title, md)) in pages.iter().enumerate() {
        if i > 0 {
            out.push_str("\n#pagebreak()\n\n");
        }
        out.push_str(&markdown_to_typst(md));
    }
    out
}

// ── Markdown → Typst converter ────────────────────────────────────────────────

fn markdown_to_typst(md: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_FOOTNOTES);

    let parser = Parser::new_ext(md, opts);
    let mut out = String::new();
    let mut in_code = false;
    let mut list_stack: Vec<Option<u64>> = Vec::new();

    for event in parser {
        match event {
            // Headings
            Event::Start(Tag::Heading { level, .. }) => {
                out.push('\n');
                for _ in 0..(level as usize) {
                    out.push('=');
                }
                out.push(' ');
            }
            Event::End(TagEnd::Heading(_)) => out.push('\n'),

            // Paragraphs
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => out.push_str("\n\n"),

            // Inline formatting
            Event::Start(Tag::Strong) => out.push('*'),
            Event::End(TagEnd::Strong) => out.push('*'),
            Event::Start(Tag::Emphasis) => out.push('_'),
            Event::End(TagEnd::Emphasis) => out.push('_'),
            Event::Start(Tag::Strikethrough) => out.push_str("#strike["),
            Event::End(TagEnd::Strikethrough) => out.push(']'),

            // Code blocks
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code = true;
                match kind {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => {
                        out.push_str(&format!("```{lang}\n"));
                    }
                    _ => out.push_str("```\n"),
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                out.push_str("```\n\n");
                in_code = false;
            }

            // Lists
            Event::Start(Tag::List(start)) => list_stack.push(start),
            Event::End(TagEnd::List(_)) => {
                list_stack.pop();
                out.push('\n');
            }
            Event::Start(Tag::Item) => {
                let indent = "  ".repeat(list_stack.len().saturating_sub(1));
                match list_stack.last() {
                    Some(Some(_)) => out.push_str(&format!("{indent}+ ")),
                    _ => out.push_str(&format!("{indent}- ")),
                }
            }
            Event::End(TagEnd::Item) if !out.ends_with('\n') => {
                out.push('\n');
            }

            // Blockquotes
            Event::Start(Tag::BlockQuote(_)) => {
                out.push_str(
                    "#block(inset: (left: 1em), stroke: (left: 2pt + gray.lighten(30%)))[",
                );
            }
            Event::End(TagEnd::BlockQuote(_)) => out.push_str("]\n\n"),

            // Links
            Event::Start(Tag::Link { dest_url, .. }) => {
                out.push_str(&format!("#link(\"{dest_url}\")["));
            }
            Event::End(TagEnd::Link) => out.push(']'),

            // Images — omit gracefully
            Event::Start(Tag::Image { .. }) | Event::End(TagEnd::Image) => {}

            // Tables
            Event::Start(Tag::Table(alignments)) => {
                out.push_str(&format!("#table(\n  columns: {},\n", alignments.len()));
            }
            Event::End(TagEnd::Table) => out.push_str(")\n\n"),
            Event::Start(Tag::TableHead | Tag::TableRow)
            | Event::End(TagEnd::TableHead | TagEnd::TableRow) => {}
            Event::Start(Tag::TableCell) => out.push_str("  ["),
            Event::End(TagEnd::TableCell) => out.push_str("],\n"),

            // Inline code
            Event::Code(text) => {
                out.push('`');
                out.push_str(&text);
                out.push('`');
            }

            // Text
            Event::Text(text) => {
                if in_code {
                    out.push_str(&text);
                } else {
                    typst_escape(&text, &mut out);
                }
            }

            Event::SoftBreak => out.push(' '),
            Event::HardBreak => out.push_str("\\\n"),
            Event::Rule => out.push_str("\n#line(length: 100%)\n\n"),
            _ => {}
        }
    }

    out
}

fn typst_escape(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '#' | '@' | '$' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
}
