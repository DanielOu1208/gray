#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkpoint {

    pub source_bytes: usize,

    pub output_lines: usize,

    pub kind: CheckpointKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointKind {

    Heading,

    Paragraph,

    CodeBlock,

    BlockQuote,

    List,

    ThematicBreak,

    Table,

    HtmlBlock,
}
