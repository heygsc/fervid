use fervid_core::{ElementNode, BuiltinType};
use swc_core::ecma::ast::Expr;

use crate::CodegenContext;

mod slot;

impl CodegenContext {
    pub fn generate_builtin(&mut self, element_node: &ElementNode, builtin_type: BuiltinType) -> Expr {
        match builtin_type {
            BuiltinType::KeepAlive => todo!(),
            BuiltinType::Slot => self.generate_slot(element_node),
            BuiltinType::Suspense => todo!(),
            BuiltinType::Teleport => todo!(),
            BuiltinType::Transition => todo!(),
            BuiltinType::TransitionGroup => todo!(),
        }
    }
}