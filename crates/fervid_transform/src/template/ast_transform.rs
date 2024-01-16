use std::borrow::Cow;

use fervid_core::{
    check_attribute_name, fervid_atom, is_from_default_slot, is_html_tag, AttributeOrBinding,
    BindingTypes, BindingsHelper, ComponentBinding, Conditional, ConditionalNodeSequence,
    ElementKind, ElementNode, FervidAtom, Interpolation, Node, PatchFlags, SfcTemplateBlock,
    StartingTag, StrOrExpr, TemplateScope, VOnDirective, VSlotDirective, VUE_BUILTINS,
};
use smallvec::SmallVec;
use swc_core::{common::DUMMY_SP, ecma::ast::Expr};

use super::{collect_vars::collect_variables, expr_transform::BindingsHelperTransform};

struct TemplateVisitor<'s> {
    bindings_helper: &'s mut BindingsHelper,
    current_scope: u32,
}

/// Transforms the AST template by using information from [`BindingsHelper`].
///
/// The transformations tackled:
/// - Optimizing the tree by removing white-space nodes;
/// - Folding the conditional nodes (`v-if`, etc.) into a single `ConditionalNode`;
/// - Transforming Js expressions by resolving variables inside them.
pub fn transform_and_record_template(
    template: &mut SfcTemplateBlock,
    bindings_helper: &mut BindingsHelper,
) {
    // Optimize conditional sequences within template root
    optimize_children(&mut template.roots, ElementKind::Element);

    // Merge more than 1 child into a separate `<template>` element so that Fragment gets generated.
    // #11: Do this only when all children are `TextNode`s.
    if template.roots.len() > 1
        && !template
            .roots
            .iter()
            .all(|r| matches!(r, Node::Text(_, _) | Node::Interpolation(_)))
    {
        let all_roots = std::mem::replace(&mut template.roots, Vec::with_capacity(1));
        let new_root = Node::Element(ElementNode {
            kind: ElementKind::Element,
            starting_tag: StartingTag {
                tag_name: fervid_atom!("template"),
                attributes: vec![],
                directives: None,
            },
            children: all_roots,
            template_scope: 0,
            patch_hints: Default::default(),
            span: template.span,
        });
        template.roots.push(new_root);
    }

    let mut template_visitor = TemplateVisitor {
        bindings_helper,
        current_scope: 0,
    };

    for node in template.roots.iter_mut() {
        node.visit_mut_with(&mut template_visitor);
    }
}

/// Optimizes the children by removing whitespace in between `ElementNode`s,
/// as well as folding `v-if`/`v-else-if`/`v-else` sequences into a `ConditionalNodeSequence`
fn optimize_children(children: &mut Vec<Node>, element_kind: ElementKind) {
    let children_len = children.len();

    // Discard children mask, limited to 128 children. 0 means to preserve the node, 1 to discard
    let mut discard_mask: u128 = 0;

    // Filter out whitespace text nodes at the beginning and end of ElementNode
    match children.first() {
        Some(Node::Text(v, _)) if v.trim().is_empty() => {
            discard_mask |= 1 << 0;
        }
        _ => {}
    }
    match children.last() {
        Some(Node::Text(v, _)) if v.trim().is_empty() => {
            discard_mask |= 1 << (children_len - 1);
        }
        _ => {}
    }

    // For removing the middle whitespace text nodes, we need sliding windows of three nodes
    for (index, window) in children.windows(3).enumerate() {
        match window {
            [Node::Element(_) | Node::Comment(_, _), Node::Text(middle, _), Node::Element(_) | Node::Comment(_, _)]
                if middle.trim().is_empty() =>
            {
                discard_mask |= 1 << (index + 1);
            }
            _ => {}
        }
    }

    // Retain based on discard_mask. If a discard bit at `index` is set to 1, the node will be dropped
    let mut index = 0;
    children.retain(|_| {
        let should_retain = discard_mask & (1 << index) == 0;
        index += 1;
        should_retain
    });

    // For components, reorder children so that named slots come first
    if matches!(element_kind, ElementKind::Component) && children.len() > 0 {
        children.sort_by(|a, b| {
            let a_is_from_default = is_from_default_slot(a);
            let b_is_from_default = is_from_default_slot(b);

            a_is_from_default.cmp(&b_is_from_default)
        });
    }

    // Merge multiple v-if/else-if/else nodes into a ConditionalNodeSequence
    if !children.is_empty() {
        let mut seq: Option<ConditionalNodeSequence> = None;
        let mut new_children = Vec::with_capacity(children.len());

        /// Finishes the sequence. Pass `child` to also push the current child
        macro_rules! finish_seq {
            () => {
                if let Some(seq) = seq.take() {
                    new_children.push(Node::ConditionalSeq(seq))
                }
            };
            ($child: expr) => {
                finish_seq!();
                new_children.push($child);
            };
        }

        // To move out of &ElementNode to ElementNode and avoid "partially moved variable" error
        macro_rules! deref_element {
            ($child: ident) => {{
                let Node::Element(child_element) = $child else {
                    unreachable!()
                };

                optimize_v_if_plus_v_for(child_element)
            }};
        }

        for mut child in children.drain(..) {
            // Only process `ElementNode`s.
            // Otherwise, when we have an `if` node, ignore `Comment`s and finish sequence.
            let Node::Element(child_element) = &mut child else {
                if let (Node::Comment(_, _), Some(_)) = (&child, seq.as_ref()) {
                    continue;
                } else {
                    finish_seq!(child);
                    continue;
                }
            };

            let Some(ref mut directives) = child_element.starting_tag.directives else {
                finish_seq!(child);
                continue;
            };

            // Check if we have a `v-if`.
            // The already existing sequence should end, and the new sequence should start.
            if let Some(v_if) = directives.v_if.take() {
                finish_seq!();
                seq = Some(ConditionalNodeSequence {
                    if_node: Box::new(Conditional {
                        condition: *v_if,
                        node: deref_element!(child),
                    }),
                    else_if_nodes: vec![],
                    else_node: None,
                });
                continue;
            }

            // Check for `v-else-if`
            if let Some(v_else_if) = directives.v_else_if.take() {
                let Some(ref mut seq) = seq else {
                    // This must be a warning, v-else-if without v-if
                    finish_seq!(child);
                    continue;
                };

                seq.else_if_nodes.push(Conditional {
                    condition: *v_else_if,
                    node: deref_element!(child),
                });
                continue;
            }

            // Check for `v-else`
            if let Some(_) = directives.v_else {
                let Some(ref mut cond_seq) = seq else {
                    // This must be a warning, v-else without v-if
                    finish_seq!(child);
                    continue;
                };

                cond_seq.else_node = Some(Box::new(deref_element!(child)));

                // `else` node always finishes the sequence
                finish_seq!();
                continue;
            }

            // No directives, just push the child
            finish_seq!(child);
        }

        finish_seq!();

        *children = new_children;
    }
}

// Optimize combined usage of conditional directives and `v-for`
// https://github.com/vuejs/core/blob/438a74aad840183286fbdb488178510f37218a73/packages/compiler-core/src/transforms/vIf.ts#L260
fn optimize_v_if_plus_v_for(mut parent: ElementNode) -> ElementNode {
    // Check that work is needed
    // This must be a `<template>` element with exactly one Element child
    if parent.children.len() != 1 || parent.starting_tag.tag_name != "template" {
        return parent;
    }

    let Some(Node::Element(child)) = parent.children.first_mut() else {
        return parent;
    };

    // There must be at most one `v-for` for both parent and child
    let parent_has_v_for = parent
        .starting_tag
        .directives
        .as_ref()
        .map_or(false, |d| d.v_for.is_some());
    let child_has_v_for = child
        .starting_tag
        .directives
        .as_ref()
        .map_or(false, |d| d.v_for.is_some());
    if parent_has_v_for && child_has_v_for {
        return parent;
    }

    // Take parent's `v-for` and give it to the child
    if parent_has_v_for {
        let Some(mut parent_directives) = parent.starting_tag.directives.take() else {
            unreachable!()
        };

        let child_directives = child
            .starting_tag
            .directives
            .get_or_insert_with(Default::default);
        child_directives.v_for = parent_directives.v_for.take();
    }

    // Take the child and return it instead
    let Some(Node::Element(child)) = parent.children.pop() else {
        unreachable!()
    };

    child
}

trait Visitor {
    fn visit_element_node(&mut self, element_node: &mut ElementNode);
    fn visit_conditional_node(&mut self, conditional_node: &mut ConditionalNodeSequence);
    fn visit_interpolation(&mut self, interpolation: &mut Interpolation);
}

trait VisitMut {
    fn visit_mut_with(&mut self, visitor: &mut impl Visitor);
}

impl<'a> Visitor for TemplateVisitor<'_> {
    fn visit_element_node(&mut self, element_node: &mut ElementNode) {
        let parent_scope = self.current_scope;
        let mut scope_to_use = parent_scope;

        // Mark the node with a correct type (element, component or built-in)
        let element_kind = self.recognize_element_kind(&element_node.starting_tag);
        let is_component = matches!(element_kind, ElementKind::Component);
        element_node.kind = element_kind;

        if is_component {
            self.maybe_resolve_component(&element_node.starting_tag.tag_name);
        }

        // Check if there is a scoping directive
        // Finds a `v-for` or `v-slot` directive when in ElementNode
        // and collects their variables into the new template scope
        if let Some(ref mut directives) = element_node.starting_tag.directives {
            let v_for = directives.v_for.as_mut();
            let v_slot = directives.v_slot.as_ref();

            // Create a new scope
            if v_for.is_some() || v_slot.is_some() {
                // New scope will have ID equal to length
                scope_to_use = self.bindings_helper.template_scopes.len() as u32;
                self.bindings_helper.template_scopes.push(TemplateScope {
                    variables: SmallVec::new(),
                    parent: parent_scope,
                });
            }

            if let Some(v_for) = v_for {
                // Get the iterator variable and collect its variables
                let mut scope = &mut self.bindings_helper.template_scopes[scope_to_use as usize];
                collect_variables(&v_for.itervar, &mut scope);

                // Transform the iterable
                let is_dynamic = self
                    .bindings_helper
                    .transform_expr(&mut v_for.iterable, scope_to_use);

                // Add patch flags
                if !is_dynamic {
                    // This is `64 /* STABLE_FRAGMENT */))`
                    // when iterable is non-dynamic (number, string) (`v-for="i in 3"`)
                    v_for.patch_flags |= PatchFlags::StableFragment;
                } else {
                    // Look for `key`. Fragment is either keyed or unkeyed.
                    let has_key = element_node
                        .starting_tag
                        .attributes
                        .iter()
                        .any(|attr| check_attribute_name(attr, "key"));

                    v_for.patch_flags |= if has_key {
                        PatchFlags::KeyedFragment
                    } else {
                        PatchFlags::UnkeyedFragment
                    };
                }
            }

            if let Some(VSlotDirective {
                value: Some(v_slot_value),
                ..
            }) = v_slot
            {
                // Collect slot bindings
                let mut scope = &mut self.bindings_helper.template_scopes[scope_to_use as usize];
                collect_variables(v_slot_value, &mut scope);
                // TODO transform slot?
            }
        }

        // Update the element's scope and the Visitor's current scope
        element_node.template_scope = scope_to_use;
        self.current_scope = scope_to_use;

        // Transform the VBind and VOn attributes
        let patch_hints = &mut element_node.patch_hints;
        for attr in element_node.starting_tag.attributes.iter_mut() {
            match attr {
                // The logic for the patch flags:
                // 1. Check if the attribute name is dynamic (`:foo` vs `:[foo]`) or ;
                //    If it is, clear the previous prop hints and set FULL_PROPS, then continue loop;
                // 2. Check if there is a Js variable in the value;
                //    If there is, check if it is a component
                // 2. Check if
                AttributeOrBinding::VBind(v_bind) => {
                    let has_bindings = self
                        .bindings_helper
                        .transform_expr(&mut v_bind.value, scope_to_use);

                    let Some(StrOrExpr::Str(ref argument)) = v_bind.argument else {
                        // This is dynamic
                        // From docs: [FULL_PROPS is] exclusive with CLASS, STYLE and PROPS.
                        patch_hints.flags &=
                            !(PatchFlags::Props | PatchFlags::Class | PatchFlags::Style);
                        patch_hints.flags |= PatchFlags::FullProps;
                        patch_hints.props.clear();
                        continue;
                    };

                    // Again, if we are FULL_PROPS already, do not add other props/class/style.
                    // Or if we do not need to add.
                    if !has_bindings || patch_hints.flags.contains(PatchFlags::FullProps) {
                        continue;
                    }

                    // Skip `key` prop
                    if argument == "key" {
                        continue;
                    }

                    // Adding `class` and `style` bindings depends on `is_component`
                    // They are added to PROPS for the components.
                    if is_component {
                        patch_hints.flags |= PatchFlags::Props;
                        patch_hints.props.push(argument.to_owned());
                        continue;
                    }

                    if argument == "class" {
                        patch_hints.flags |= PatchFlags::Class;
                    } else if argument == "style" {
                        patch_hints.flags |= PatchFlags::Style;
                    } else {
                        patch_hints.flags |= PatchFlags::Props;
                        patch_hints.props.push(argument.to_owned());
                    }
                }

                AttributeOrBinding::VOn(VOnDirective {
                    handler: Some(ref mut handler),
                    ..
                }) => {
                    self.bindings_helper.transform_expr(handler, scope_to_use);
                }

                _ => {}
            }
        }

        // Transform the directives
        if let Some(ref mut directives) = element_node.starting_tag.directives {
            macro_rules! maybe_transform {
                ($key: ident) => {
                    match directives.$key.as_mut() {
                        Some(expr) => self.bindings_helper.transform_expr(expr, scope_to_use),
                        None => false,
                    }
                };
            }
            maybe_transform!(v_html);
            maybe_transform!(v_memo);
            maybe_transform!(v_show);
            maybe_transform!(v_text);

            for v_model in directives.v_model.iter_mut() {
                self.bindings_helper
                    .transform_v_model(v_model, scope_to_use, patch_hints);
            }
        }

        // Merge conditional nodes and clean up whitespace
        optimize_children(&mut element_node.children, element_kind);

        // Patch flag for HTML elements which only contain interpolation and text,
        // e.g. `<p>{{ msg }}</p>`.
        // Does not apply to components or child-less elements
        let mut is_children_text_only =
            matches!(element_kind, ElementKind::Element) && !element_node.children.is_empty();
        let mut has_dynamic_interpolation = false;

        // Recursively visit children
        for child in element_node.children.iter_mut() {
            child.visit_mut_with(self);

            match child {
                // When Elements are present, TEXT patch flag does not apply
                Node::Element(_) | Node::ConditionalSeq(_) => {
                    is_children_text_only = false;
                }

                // TEXT patch flag only applies when there is an interpolation with a patch flag
                Node::Interpolation(interpolation) => {
                    has_dynamic_interpolation |= interpolation.patch_flag;
                }

                Node::Text(_, _) | Node::Comment(_, _) => {}
            }
        }

        // Apply TEXT patch flag
        if is_children_text_only && has_dynamic_interpolation {
            patch_hints.flags |= PatchFlags::Text;
        }

        // Restore the parent scope
        self.current_scope = parent_scope;
    }

    fn visit_conditional_node(&mut self, conditional_node: &mut ConditionalNodeSequence) {
        // In this function, conditions are transformed first
        // without updating the template scope and collecting its variables.
        // I believe this is a correct way of doing it, because in VDOM the condition
        // wraps around the node (`condition ? if_node : else_node`).
        // However, I am not too sure about the `v-if` & `v-slot` combined usage.

        self.bindings_helper
            .transform_expr(&mut conditional_node.if_node.condition, self.current_scope);
        self.visit_element_node(&mut conditional_node.if_node.node);

        for else_if_node in conditional_node.else_if_nodes.iter_mut() {
            self.bindings_helper
                .transform_expr(&mut else_if_node.condition, self.current_scope);
            self.visit_element_node(&mut else_if_node.node);
        }

        if let Some(ref mut else_node) = conditional_node.else_node {
            self.visit_element_node(else_node);
        }
    }

    fn visit_interpolation(&mut self, interpolation: &mut Interpolation) {
        interpolation.template_scope = self.current_scope;

        let has_js = self
            .bindings_helper
            .transform_expr(&mut interpolation.value, self.current_scope);

        interpolation.patch_flag = has_js;
    }
}

impl TemplateVisitor<'_> {
    // TODO Maybe do this in parser instead, because it sometimes needs this info
    fn recognize_element_kind(&self, starting_tag: &StartingTag) -> ElementKind {
        let tag_name = &starting_tag.tag_name;

        // First, check for a built-in
        if let Some(builtin_type) = VUE_BUILTINS.get(&tag_name) {
            // Special case for `<component>`. If it does not have `is`, this is not a built-in
            if tag_name.eq("component") {
                let has_is = starting_tag
                    .attributes
                    .iter()
                    .any(|attr| check_attribute_name(attr, "is"));

                if !has_is {
                    return ElementKind::Component;
                }
            }

            return ElementKind::Builtin(*builtin_type);
        }

        // Then check if this is an HTML tag
        if is_html_tag(&starting_tag.tag_name) {
            ElementKind::Element
        } else {
            ElementKind::Component
        }
    }

    /// Fuzzy-matches the component name to a binding name
    fn maybe_resolve_component(&mut self, tag_name: &FervidAtom) {
        // Check the existing resolutions.
        // Do nothing if found, regardless if it was previously resolved or not,
        // because codegen will handle the runtime resolution.
        if self.bindings_helper.components.contains_key(tag_name) {
            return;
        }

        // `component-name`s like that should be transformed to `ComponentName`s
        let searched: Cow<str> = if tag_name.contains('-') {
            let mut transformed_name = String::with_capacity(tag_name.len());
            for word in tag_name.split('-') {
                let first_char = word.chars().next();
                if let Some(ch) = first_char {
                    // Uppercase the first char and append to buf
                    for ch_component in ch.to_uppercase() {
                        transformed_name.push(ch_component);
                    }

                    // Push the rest of the word
                    transformed_name.push_str(&word[ch.len_utf8()..]);
                }
            }

            Cow::Owned(transformed_name)
        } else {
            Cow::Borrowed(&tag_name)
        };

        let found = self
            .bindings_helper
            .setup_bindings
            .iter()
            .find(|binding| binding.0 == searched);

        if let Some(found) = found {
            let mut resolved_to = Expr::Ident(swc_core::ecma::ast::Ident {
                span: DUMMY_SP,
                sym: found.0.to_owned(),
                optional: false,
            });

            // For `Component` binding types, do not transform.
            // TODO I am not sure about `Imported` though,
            // the official compiler sees them as if `SetupMaybeRef` and transforms.
            if !matches!(found.1, BindingTypes::Component) {
                self.bindings_helper
                    .transform_expr(&mut resolved_to, self.current_scope);
            }

            // Was resolved
            self.bindings_helper.components.insert(
                tag_name.to_owned(),
                ComponentBinding::Resolved(Box::new(resolved_to)),
            );
        } else {
            // Was not resolved
            self.bindings_helper
                .components
                .insert(tag_name.to_owned(), ComponentBinding::Unresolved);
        }
    }
}

impl VisitMut for Node {
    fn visit_mut_with(&mut self, visitor: &mut impl Visitor) {
        match self {
            Node::Element(el) => visitor.visit_element_node(el),
            Node::ConditionalSeq(cond) => visitor.visit_conditional_node(cond),
            Node::Interpolation(interpolation) => visitor.visit_interpolation(interpolation),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use fervid_core::{ElementKind, Node, PatchHints, SetupBinding, VForDirective, VueDirectives};
    use swc_core::{common::DUMMY_SP, ecma::ast::Expr};

    use crate::test_utils::{parser::parse_javascript_expr, to_str};

    use super::*;

    /// Special case: `<component>` without `is` attribute is not a builtin
    #[test]
    fn it_distinguishes_component_builtin_and_not() {
        let starting_tag = StartingTag {
            tag_name: "component".into(),
            attributes: vec![],
            directives: None,
        };

        let mut bindings_helper = Default::default();
        let template_visitor = TemplateVisitor {
            bindings_helper: &mut bindings_helper,
            current_scope: 0,
        };
        assert!(matches!(
            template_visitor.recognize_element_kind(&starting_tag),
            ElementKind::Component
        ));
    }

    #[test]
    fn it_folds_basic_seq() {
        // <template><div>
        //   text
        //   <h1 v-if="true">if</h1>
        //   <h2 v-else-if="foo">else-if</h2>
        //   <h3 v-else>else</h3>
        // </div></template>
        let mut sfc_template = SfcTemplateBlock {
            lang: "html".into(),
            roots: vec![Node::Element(ElementNode {
                starting_tag: StartingTag {
                    tag_name: "div".into(),
                    attributes: vec![],
                    directives: None,
                },
                children: vec![text_node(), if_node(), else_if_node(), else_node()],
                template_scope: 0,
                kind: ElementKind::Element,
                patch_hints: Default::default(),
                span: DUMMY_SP,
            })],
            span: DUMMY_SP,
        };

        transform_and_record_template(&mut sfc_template, &mut Default::default());

        // Template roots: one div
        assert_eq!(1, sfc_template.roots.len());
        let Node::Element(ref div) = sfc_template.roots[0] else {
            panic!("Root is not an element")
        };

        // Text and conditional seq
        assert_eq!(2, div.children.len());
        check_text_node(&div.children[0]);
        let Node::ConditionalSeq(seq) = &div.children[1] else {
            panic!("Not a conditional sequence")
        };

        // <h1 v-if="true">if</h1>
        check_if_node(&seq.if_node);

        // <h2 v-else-if="foo">else-if</h3>
        assert_eq!(1, seq.else_if_nodes.len());
        check_else_if_node(&seq.else_if_nodes[0]);

        // <h3 v-else>else</h3>
        check_else_node(seq.else_node.as_ref());
    }

    #[test]
    fn it_folds_roots() {
        // <template>
        //   <h1 v-if="true">if</h1>
        //   <h2 v-else-if="foo">else-if</h2>
        //   <h3 v-else>else</h3>
        // </template>
        let mut sfc_template = SfcTemplateBlock {
            lang: "html".into(),
            roots: vec![if_node(), else_if_node(), else_node()],
            span: DUMMY_SP,
        };

        transform_and_record_template(&mut sfc_template, &mut Default::default());

        // Template roots: one conditional sequence
        assert_eq!(1, sfc_template.roots.len());
        let Node::ConditionalSeq(ref seq) = sfc_template.roots[0] else {
            panic!("Root is not a conditional sequence")
        };

        // <h1 v-if="true">if</h1>
        check_if_node(&seq.if_node);

        // <h2 v-else-if="foo">else-if</h3>
        assert_eq!(1, seq.else_if_nodes.len());
        check_else_if_node(&seq.else_if_nodes[0]);

        // <h3 v-else>else</h3>
        check_else_node(seq.else_node.as_ref());
    }

    #[test]
    fn it_folds_multiple_ifs() {
        // <template>
        //   <h1 v-if="true">if</h1>
        //   <h1 v-if="true">if</h1>
        // </template>
        let mut sfc_template = SfcTemplateBlock {
            lang: "html".into(),
            roots: vec![if_node(), if_node()],
            span: DUMMY_SP,
        };

        transform_and_record_template(&mut sfc_template, &mut Default::default());

        // Template roots: two conditional sequences inside one root
        assert_eq!(1, sfc_template.roots.len());
        let Node::Element(ref root) = sfc_template.roots[0] else {
            panic!("root is not an element")
        };
        let Node::ConditionalSeq(ref seq) = root.children[0] else {
            panic!("root.children[0] is not a conditional sequence")
        };
        // <h1 v-if="true">if</h1>
        check_if_node(&seq.if_node);

        let Node::ConditionalSeq(ref seq) = root.children[1] else {
            panic!("root.children[1] not a conditional sequence")
        };
        // <h1 v-if="true">if</h1>
        check_if_node(&seq.if_node);
    }

    #[test]
    fn it_folds_multiple_else_ifs() {
        // <template>
        //   <h1 v-if="true">if</h1>
        //   <h2 v-else-if="foo">else-if</h2>
        //   <h1 v-if="true">if</h1>
        //   <h2 v-else-if="foo">else-if</h2>
        // </template>
        let mut sfc_template = SfcTemplateBlock {
            lang: "html".into(),
            roots: vec![if_node(), else_if_node(), if_node(), else_if_node()],
            span: DUMMY_SP,
        };

        transform_and_record_template(&mut sfc_template, &mut Default::default());

        // Template roots: two conditional sequences inside one root
        assert_eq!(1, sfc_template.roots.len());
        let Node::Element(ref root) = sfc_template.roots[0] else {
            panic!("root is not an element")
        };
        let Node::ConditionalSeq(ref seq) = root.children[0] else {
            panic!("roots[0] is not a conditional sequence")
        };
        check_if_node(&seq.if_node);
        check_else_if_node(&seq.else_if_nodes[0]);

        let Node::ConditionalSeq(ref seq) = root.children[1] else {
            panic!("roots[1] not a conditional sequence")
        };
        check_if_node(&seq.if_node);
        check_else_if_node(&seq.else_if_nodes[0]);
    }

    #[test]
    fn it_leaves_bad_nodes() {
        // <template>
        //   <h2 v-else-if="foo">else-if</h2>
        //   <h3 v-else>else</h3>
        // </template>
        let mut sfc_template = SfcTemplateBlock {
            lang: "html".into(),
            roots: vec![else_if_node(), else_node()],
            span: DUMMY_SP,
        };

        transform_and_record_template(&mut sfc_template, &mut Default::default());

        // Template root children: still two
        assert_eq!(1, sfc_template.roots.len());
        let Node::Element(ref root) = sfc_template.roots[0] else {
            panic!("root is not an element")
        };
        assert!(matches!(root.children[0], Node::Element(_)));
        assert!(matches!(root.children[1], Node::Element(_)));
    }

    #[test]
    fn it_merges_roots() {
        // #11: Should not get merged
        // <template>
        //   hello {{ 1 + 1 }}
        // </template>
        let mut sfc_template = SfcTemplateBlock {
            lang: "html".into(),
            roots: vec![
                Node::Text("text".into(), DUMMY_SP),
                Node::Interpolation(Interpolation {
                    value: js("1 + 1"),
                    template_scope: 0,
                    patch_flag: false,
                    span: DUMMY_SP,
                }),
            ],
            span: DUMMY_SP,
        };
        transform_and_record_template(&mut sfc_template, &mut Default::default());
        assert_eq!(2, sfc_template.roots.len());

        // Should get merged
        // <template>
        //   hello {{ 1 + 1 }}
        //   <div />
        // </template>
        let mut sfc_template = SfcTemplateBlock {
            lang: "html".into(),
            roots: vec![
                Node::Text("text".into(), DUMMY_SP),
                Node::Interpolation(Interpolation {
                    value: js("1 + 1"),
                    template_scope: 0,
                    patch_flag: false,
                    span: DUMMY_SP,
                }),
                Node::Element(ElementNode {
                    kind: ElementKind::Element,
                    starting_tag: StartingTag {
                        tag_name: "div".into(),
                        attributes: vec![],
                        directives: None,
                    },
                    children: vec![],
                    template_scope: 0,
                    patch_hints: PatchHints::default(),
                    span: DUMMY_SP,
                }),
            ],
            span: DUMMY_SP,
        };
        transform_and_record_template(&mut sfc_template, &mut Default::default());
        assert_eq!(1, sfc_template.roots.len());
    }

    #[test]
    fn it_handles_complex_cases() {
        // <template><div>
        //   text
        //   <h1 v-if="true">if</h1>
        //   text
        //   <h1 v-if="true">if</h1>
        //   <h2 v-else-if="foo">else-if</h2>
        //   text
        //   <h1 v-if="true">if</h1>
        //   <h3 v-else>else</h3>
        // </div></template>
        let mut sfc_template = SfcTemplateBlock {
            lang: "html".into(),
            roots: vec![Node::Element(ElementNode {
                starting_tag: StartingTag {
                    tag_name: "div".into(),
                    attributes: vec![],
                    directives: None,
                },
                children: vec![
                    text_node(),
                    if_node(),
                    text_node(),
                    if_node(),
                    else_if_node(),
                    text_node(),
                    if_node(),
                    else_node(),
                ],
                template_scope: 0,
                kind: ElementKind::Element,
                patch_hints: Default::default(),
                span: DUMMY_SP,
            })],
            span: DUMMY_SP,
        };

        transform_and_record_template(&mut sfc_template, &mut Default::default());

        // Template roots: one div
        assert_eq!(1, sfc_template.roots.len());
        let Node::Element(ref div) = sfc_template.roots[0] else {
            panic!("Root is not an element")
        };

        // Text and conditional seq
        assert_eq!(6, div.children.len());
        check_text_node(&div.children[0]);
        check_text_node(&div.children[2]);
        check_text_node(&div.children[4]);
        assert!(matches!(&div.children[1], Node::ConditionalSeq(_)));
        assert!(matches!(&div.children[3], Node::ConditionalSeq(_)));
        assert!(matches!(&div.children[5], Node::ConditionalSeq(_)));
    }

    #[test]
    fn it_ignores_node_without_conditional_directives() {
        let no_directives1 = Node::Element(ElementNode {
            starting_tag: StartingTag {
                tag_name: "test-component".into(),
                attributes: vec![],
                directives: Some(Box::new(VueDirectives {
                    ..Default::default()
                })),
            },
            children: vec![],
            template_scope: 0,
            kind: ElementKind::Element,
            patch_hints: Default::default(),
            span: DUMMY_SP,
        });

        let no_directives2 = Node::Element(ElementNode {
            starting_tag: StartingTag {
                tag_name: "div".into(),
                attributes: vec![],
                directives: Some(Box::new(VueDirectives {
                    ..Default::default()
                })),
            },
            children: vec![Node::Text("hello".into(), DUMMY_SP)],
            template_scope: 0,
            kind: ElementKind::Element,
            patch_hints: Default::default(),
            span: DUMMY_SP,
        });

        let mut sfc_template = SfcTemplateBlock {
            lang: "html".into(),
            roots: vec![no_directives1, no_directives2],
            span: DUMMY_SP,
        };

        transform_and_record_template(&mut sfc_template, &mut Default::default());

        // Template root: both children nodes are still present
        assert_eq!(1, sfc_template.roots.len());
        let Node::Element(ref root) = sfc_template.roots[0] else {
            panic!("root is not an element")
        };
        assert_eq!(2, root.children.len());
    }

    #[test]
    fn it_optimizes_nested_fragments() {
        // For cloning
        // <p>text</p>
        let p = ElementNode {
            kind: ElementKind::Element,
            starting_tag: StartingTag {
                tag_name: "p".into(),
                attributes: vec![],
                directives: Some(Default::default()),
            },
            children: vec![Node::Text("text".into(), DUMMY_SP)],
            template_scope: 0,
            patch_hints: Default::default(),
            span: DUMMY_SP,
        };
        // <div v-if="false"></div>
        let div = ElementNode {
            kind: ElementKind::Element,
            starting_tag: StartingTag {
                tag_name: "div".into(),
                attributes: vec![],
                directives: Some(Box::new(VueDirectives {
                    v_if: Some(js("false")),
                    ..Default::default()
                })),
            },
            children: vec![Node::Text("text".into(), DUMMY_SP)],
            template_scope: 0,
            patch_hints: Default::default(),
            span: DUMMY_SP,
        };
        // <template></template>
        let tmpl = ElementNode {
            kind: ElementKind::Element,
            starting_tag: StartingTag {
                tag_name: "template".into(),
                attributes: vec![],
                directives: Some(Default::default()),
            },
            children: vec![],
            template_scope: 0,
            patch_hints: Default::default(),
            span: DUMMY_SP,
        };
        let sfc_tmpl = SfcTemplateBlock {
            lang: "html".into(),
            roots: vec![],
            span: DUMMY_SP,
        };

        // Convenience
        let prepare = |template_directives: Option<Box<VueDirectives>>,
                       p_directives: Option<Box<VueDirectives>>,
                       include_div: bool| {
            let mut p = p.clone();
            p.starting_tag.directives = p_directives;

            let mut template = tmpl.clone();
            template.starting_tag.directives = template_directives;
            template.children.push(Node::Element(p));

            let mut sfc_template = sfc_tmpl.clone();
            if include_div {
                sfc_template.roots.push(Node::Element(div.clone()));
            }
            sfc_template.roots.push(Node::Element(template));
            transform_and_record_template(&mut sfc_template, &mut Default::default());

            let Some(Node::ConditionalSeq(cond)) = sfc_template.roots.pop() else {
                panic!("root is not a conditional seq")
            };

            cond
        };

        // Convenience
        macro_rules! directives {
            ($($directive:ident: $value:expr),* $(,)?) => {
                Box::new(VueDirectives {
                    $($directive: $value,)*
                    ..Default::default()
                })
            };
        }

        // <template v-if="val"><p>text</p></template>
        {
            let cond = prepare(Some(directives!(v_if: Some(js("val")))), None, false);

            // Folded to `<p v-if="val">text</p>`
            assert!(cond.if_node.node.starting_tag.tag_name == "p");
            assert!(cond
                .if_node
                .node
                .children
                .first()
                .is_some_and(|v| matches!(v, Node::Text(_, _))))
        };

        // <template v-if="val" v-for="i in 3"><p>text</p></template>
        {
            let cond = prepare(
                Some(
                    directives!(v_if: Some(js("val")), v_for: Some(VForDirective {
                        iterable: js("3"),
                        itervar: js("i"),
                        patch_flags: Default::default(),
                        span: DUMMY_SP,
                    })),
                ),
                None,
                false,
            );

            // Folded to `<p v-if="val" v-for="i in 3">text</p>`
            let cond_node = &cond.if_node.node;
            assert!(cond_node.starting_tag.tag_name == "p");
            assert!(cond_node
                .children
                .first()
                .is_some_and(|v| matches!(v, Node::Text(_, _))));
            assert!(cond_node
                .starting_tag
                .directives
                .as_ref()
                .is_some_and(|d| d.v_for.is_some()));
        };

        // <template v-if="val"><p v-for="j in 3">text</p></template>
        {
            let cond = prepare(
                Some(directives!(v_if: Some(js("val")))),
                Some(directives!(v_for: Some(VForDirective {
                    iterable: js("3"),
                    itervar: js("j"),
                    patch_flags: Default::default(),
                    span: DUMMY_SP,
                }))),
                false,
            );

            // Folded to `<p v-if="val" v-for="i in 3">text</p>`
            let cond_node = &cond.if_node.node;
            assert!(cond_node.starting_tag.tag_name == "p");
            assert!(cond_node
                .children
                .first()
                .is_some_and(|v| matches!(v, Node::Text(_, _))));
            assert!(cond_node
                .starting_tag
                .directives
                .as_ref()
                .is_some_and(|d| d.v_for.is_some()));
        };

        // <template v-if="val" v-for="i in 3"><p v-for="j in 3">text</p></template>
        {
            let cond = prepare(
                Some(
                    directives!(v_if: Some(js("val")), v_for: Some(VForDirective {
                        iterable: js("3"),
                        itervar: js("i"),
                        patch_flags: Default::default(),
                        span: DUMMY_SP,
                    })),
                ),
                Some(directives!(v_for: Some(VForDirective {
                    iterable: js("3"),
                    itervar: js("j"),
                    patch_flags: Default::default(),
                    span: DUMMY_SP,
                }))),
                false,
            );

            // Not folded
            let cond_node = &cond.if_node.node;
            assert!(cond_node.starting_tag.tag_name == "template");
            assert!(cond_node
                .starting_tag
                .directives
                .as_ref()
                .is_some_and(|d| d.v_for.is_some()));

            let Some(Node::Element(first_child)) = cond_node.children.first() else {
                panic!("First child should be an element")
            };
            assert!(first_child.starting_tag.tag_name == "p");
            assert!(first_child
                .starting_tag
                .directives
                .as_ref()
                .is_some_and(|d| d.v_for.is_some()));
        };

        // <div v-if="false"></div>
        // <template v-else-if="val"><p>text</p></template>
        {
            let cond = prepare(Some(directives!(v_else_if: Some(js("val")))), None, true);

            // Folded to `<div v-if="false"></div><p v-else-if="val">text</p>`
            assert!(cond.if_node.node.starting_tag.tag_name == "div");
            let else_if_node = &cond.else_if_nodes.first().expect("Should exist").node;
            assert!(else_if_node.starting_tag.tag_name == "p");
            assert!(else_if_node
                .children
                .first()
                .is_some_and(|v| matches!(v, Node::Text(_, _))));
        };

        // <div v-if="false"></div>
        // <template v-else><p>text</p></template>
        {
            let cond = prepare(Some(directives!(v_else: Some(()))), None, true);

            // Folded to `<div v-if="false"></div><p v-else-if="val">text</p>`
            assert!(cond.if_node.node.starting_tag.tag_name == "div");
            let else_node = cond.else_node.as_ref().expect("Should exist");
            assert!(else_node.starting_tag.tag_name == "p");
            assert!(else_node
                .children
                .first()
                .is_some_and(|v| matches!(v, Node::Text(_, _))));
        };
    }

    #[test]
    fn it_resolves_components() {
        // `TestComponent` binding
        let mut bindings_helper = BindingsHelper::default();
        bindings_helper.setup_bindings.push(SetupBinding(
            fervid_atom!("TestComponent"),
            BindingTypes::Component,
        ));

        let mut template_visitor = TemplateVisitor {
            bindings_helper: &mut bindings_helper,
            current_scope: 0,
        };

        let kebab_case = fervid_atom!("test-component");
        template_visitor.maybe_resolve_component(&kebab_case);
        assert!(matches!(
            template_visitor.bindings_helper.components.get(&kebab_case),
            Some(ComponentBinding::Resolved(_))
        ));

        let pascal_case = fervid_atom!("TestComponent");
        template_visitor.maybe_resolve_component(&pascal_case);
        assert!(matches!(
            template_visitor
                .bindings_helper
                .components
                .get(&pascal_case),
            Some(ComponentBinding::Resolved(_))
        ));

        let unresolved = fervid_atom!("UnresolvedComponent");
        template_visitor.maybe_resolve_component(&unresolved);
        assert!(matches!(
            template_visitor.bindings_helper.components.get(&unresolved),
            Some(ComponentBinding::Unresolved)
        ));
    }

    // text
    fn text_node() -> Node {
        Node::Text("text".into(), DUMMY_SP)
    }

    fn check_text_node(node: &Node) {
        assert!(matches!(node, Node::Text(text, DUMMY_SP) if text == "text"));
    }

    // <h1 v-if="true">if</h1>
    fn if_node() -> Node {
        Node::Element(ElementNode {
            starting_tag: StartingTag {
                tag_name: "h1".into(),
                attributes: vec![],
                directives: Some(Box::new(VueDirectives {
                    v_if: Some(js("true")),
                    ..Default::default()
                })),
            },
            children: vec![Node::Text("if".into(), DUMMY_SP)],
            template_scope: 0,
            kind: ElementKind::Element,
            patch_hints: Default::default(),
            span: DUMMY_SP,
        })
    }

    fn check_if_node(if_node: &Conditional) {
        assert_eq!("true", to_str(&if_node.condition));
        assert!(matches!(
            &if_node.node,
            ElementNode {
                starting_tag: StartingTag {
                    tag_name,
                    ..
                },
                ..
            } if tag_name == "h1"
        ));
    }

    // <h2 v-else-if="foo">else-if</h3>
    fn else_if_node() -> Node {
        Node::Element(ElementNode {
            starting_tag: StartingTag {
                tag_name: "h2".into(),
                attributes: vec![],
                directives: Some(Box::new(VueDirectives {
                    v_else_if: Some(js("foo")),
                    ..Default::default()
                })),
            },
            children: vec![Node::Text("else-if".into(), DUMMY_SP)],
            template_scope: 0,
            kind: ElementKind::Element,
            patch_hints: Default::default(),
            span: DUMMY_SP,
        })
    }

    fn check_else_if_node(else_if_node: &Conditional) {
        // condition, then node
        assert_eq!("_ctx.foo", to_str(&else_if_node.condition));
        assert!(matches!(
            &else_if_node.node,
            ElementNode {
                starting_tag: StartingTag {
                    tag_name,
                    ..
                },
                ..
            } if tag_name == "h2"
        ));
    }

    // <h3 v-else>else</h3>
    fn else_node() -> Node {
        Node::Element(ElementNode {
            starting_tag: StartingTag {
                tag_name: "h3".into(),
                attributes: vec![],
                directives: Some(Box::new(VueDirectives {
                    v_else: Some(()),
                    ..Default::default()
                })),
            },
            children: vec![Node::Text("else".into(), DUMMY_SP)],
            template_scope: 0,
            kind: ElementKind::Element,
            patch_hints: Default::default(),
            span: DUMMY_SP,
        })
    }

    fn check_else_node(else_node: Option<&Box<ElementNode>>) {
        let else_node = else_node.expect("Must have else node");
        assert!(matches!(
            &**else_node,
            ElementNode {
                starting_tag: StartingTag {
                    tag_name,
                    ..
                },
                ..
            } if tag_name == "h3"
        ));
    }

    fn js(raw: &str) -> Box<Expr> {
        parse_javascript_expr(raw, 0, Default::default()).unwrap().0
    }
}
