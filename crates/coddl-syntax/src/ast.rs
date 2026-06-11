//! Typed AST view over the concrete syntax tree.
//!
//! Each AST node here is a thin newtype wrapping a [`SyntaxNode`]; the
//! [`AstNode`] trait below mediates the cast from raw syntax to typed
//! view. Walking the AST is walking the CST through a typed lens — the
//! tree storage is the same; the types make access ergonomic and
//! type-checked.
//!
//! The wrapper layer is essentially zero-cost: an `AstNode` newtype is
//! just a `SyntaxNode`, and the cast is one tag comparison.

use crate::cst::SyntaxNode;
use crate::syntax_kind::SyntaxKind;

/// Trait implemented by every typed AST node.
///
/// `cast` is the falling-into-place operation: given any
/// [`SyntaxNode`], it returns `Some(Self)` iff the node's kind matches
/// the AST type. `syntax` recovers the underlying CST node so callers
/// can drop back into raw-syntax mode (for span queries, child
/// iteration, etc.).
pub trait AstNode: Sized {
    fn can_cast(kind: SyntaxKind) -> bool;
    fn cast(syntax: SyntaxNode) -> Option<Self>;
    fn syntax(&self) -> &SyntaxNode;
}

/// The whole file. Wraps a [`SyntaxKind::ROOT`] node.
#[derive(Debug, Clone)]
pub struct Root {
    syntax: SyntaxNode,
}

impl AstNode for Root {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ROOT
    }
    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl Root {
    /// Iterate the top-level items in source order.
    ///
    /// Top-level items are program declarations, operator
    /// declarations, type declarations, relvar declarations, and
    /// constraint declarations — any node whose kind is one of those.
    /// Trivia and `PARSE_ERROR` placeholders are skipped here.
    pub fn items(&self) -> impl Iterator<Item = Item> + '_ {
        self.syntax.children().filter_map(Item::cast)
    }
}

/// Top-level item variants. Each carries the underlying syntax node;
/// downstream passes match on the variant and then descend.
#[derive(Debug, Clone)]
pub enum Item {
    ProgramDecl(ProgramDecl),
    OperDecl(OperDecl),
    TypeDecl(TypeDecl),
    RelvarDecl(RelvarDecl),
    ConstraintDecl(ConstraintDecl),
}

impl Item {
    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Some(match syntax.kind() {
            SyntaxKind::PROGRAM_DECL => Item::ProgramDecl(ProgramDecl { syntax }),
            SyntaxKind::OPER_DECL => Item::OperDecl(OperDecl { syntax }),
            SyntaxKind::TYPE_DECL => Item::TypeDecl(TypeDecl { syntax }),
            SyntaxKind::RELVAR_DECL => Item::RelvarDecl(RelvarDecl { syntax }),
            SyntaxKind::CONSTRAINT_DECL => Item::ConstraintDecl(ConstraintDecl { syntax }),
            _ => return None,
        })
    }

    pub fn syntax(&self) -> &SyntaxNode {
        match self {
            Item::ProgramDecl(d) => &d.syntax,
            Item::OperDecl(d) => &d.syntax,
            Item::TypeDecl(d) => &d.syntax,
            Item::RelvarDecl(d) => &d.syntax,
            Item::ConstraintDecl(d) => &d.syntax,
        }
    }
}

// Top-level item wrappers. Methods that read specific children land
// alongside the parser productions for each shape.

#[derive(Debug, Clone)]
pub struct ProgramDecl {
    syntax: SyntaxNode,
}
impl AstNode for ProgramDecl {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::PROGRAM_DECL
    }
    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

#[derive(Debug, Clone)]
pub struct OperDecl {
    syntax: SyntaxNode,
}
impl AstNode for OperDecl {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::OPER_DECL
    }
    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

#[derive(Debug, Clone)]
pub struct TypeDecl {
    syntax: SyntaxNode,
}
impl AstNode for TypeDecl {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::TYPE_DECL
    }
    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

#[derive(Debug, Clone)]
pub struct RelvarDecl {
    syntax: SyntaxNode,
}
impl AstNode for RelvarDecl {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::RELVAR_DECL
    }
    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

#[derive(Debug, Clone)]
pub struct ConstraintDecl {
    syntax: SyntaxNode,
}
impl AstNode for ConstraintDecl {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::CONSTRAINT_DECL
    }
    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
