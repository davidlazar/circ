//! The C front-end

mod ast_utils;
mod parser;
mod term;
mod types;

use super::{FrontEnd, Mode};
use crate::circify::{Circify, Loc, Val};
use crate::front::c::ast_utils::*;
use crate::front::c::term::*;
use crate::front::c::types::*;
use crate::front::field_list::FieldList;
use crate::ir::opt::cfold::fold;
use crate::ir::proof::ConstraintMetadata;
use crate::ir::term::*;
use lang_c::ast::*;
use lang_c::span::Node;
use log::debug;

use std::collections::HashMap;
use std::fmt::Display;
use std::path::PathBuf;

use crate::front::PROVER_VIS;

/// Inputs to the C compiler
pub struct Inputs {
    /// The file to look for `main` in.
    pub file: PathBuf,
    /// The mode to generate for (MPC or proof). Effects visibility.
    pub mode: Mode,
}

/// The C front-end. Implements [FrontEnd].
pub struct C;

impl FrontEnd for C {
    type Inputs = Inputs;
    fn gen(i: Inputs) -> Computation {
        let parser = parser::CParser::new();
        let p = parser.parse_file(&i.file).unwrap();
        let mut g = CGen::new(i.mode, p.unit);
        g.visit_files();
        g.entry_fn("main");
        g.circ.consume().borrow().clone()
    }
}

/// Structure for holding n-dimension array indicies.
#[derive(Clone)]
pub struct IndexTerm {
    /// Base array term
    pub base: CTerm,
    /// Vec of indicies to access into an n-dimension array
    pub indices: Vec<CTerm>,
}

#[derive(Clone)]
enum CLoc {
    Var(Loc),
    Member(Box<CLoc>, String),
    Idx(Box<CLoc>, CTerm),
}

impl CLoc {
    fn loc(&self) -> &Loc {
        match self {
            CLoc::Var(l) => l,
            CLoc::Idx(i, _) => i.loc(),
            CLoc::Member(i, _) => i.loc(),
        }
    }
}

struct CGen {
    circ: Circify<Ct>,
    mode: Mode,
    tu: TranslationUnit,
    structs: HashMap<String, Ty>,
    functions: HashMap<String, FnInfo>,
    typedefs: HashMap<String, Ty>,
}

impl CGen {
    fn new(mode: Mode, tu: TranslationUnit) -> Self {
        let this = Self {
            circ: Circify::new(Ct::new()),
            mode,
            tu,
            structs: HashMap::default(),
            functions: HashMap::default(),
            typedefs: HashMap::default(),
        };
        this.circ
            .cir_ctx()
            .cs
            .borrow_mut()
            .metadata
            .add_prover_and_verifier();
        this
    }

    /// Unwrap a result of an error and abort
    fn err<E: Display>(&self, e: E) -> ! {
        println!("Error: {}", e);
        std::process::exit(1)
    }

    /// Unwrap result of a computation
    /// TODO: Add span for debugging
    fn unwrap<CTerm, E: Display>(&self, r: Result<CTerm, E>) -> CTerm {
        r.unwrap_or_else(|e| self.err(e))
    }

    /// TODO: Refactor with s_type_
    pub fn d_type_(&mut self, ds: &[Node<DeclarationSpecifier>]) -> Option<Ty> {
        assert!(!ds.is_empty());
        let res: Vec<Option<Ty>> = ds
            .iter()
            .map(|d| match &d.node {
                DeclarationSpecifier::TypeSpecifier(t) => self.type_(&t.node),
                _ => unimplemented!("Unimplemented declaration type: {:#?}", d),
            })
            .collect();
        compress_type(res)
    }

    pub fn s_type_(&mut self, ss: &[Node<SpecifierQualifier>]) -> Option<Ty> {
        assert!(!ss.is_empty());
        let res: Vec<Option<Ty>> = ss
            .iter()
            .map(|s| match &s.node {
                SpecifierQualifier::TypeSpecifier(t) => self.type_(&t.node),
                _ => unimplemented!("Unimplemented specifier type: {:#?}", s),
            })
            .collect();
        compress_type(res)
    }

    /// TODO: Refactor with s_type_ / d_type_
    fn type_(&mut self, t: &TypeSpecifier) -> Option<Ty> {
        return match t {
            TypeSpecifier::Void => None,
            TypeSpecifier::Int => Some(Ty::Int(true, 32)),
            TypeSpecifier::Unsigned => Some(Ty::Int(false, 32)),
            TypeSpecifier::Long => Some(Ty::Int(true, 32)), // TODO: not 32 bits
            TypeSpecifier::Bool => Some(Ty::Bool),
            TypeSpecifier::TypedefName(td) => {
                let name = &td.node.name;
                if self.typedefs.contains_key(name) {
                    Some(self.typedefs[name].clone())
                } else {
                    panic!("Typedef not defined: {}", name);
                }
            }
            TypeSpecifier::Struct(s) => {
                let StructType {
                    kind: _kind,
                    identifier,
                    declarations,
                } = &s.node;

                let name = match identifier {
                    Some(name) => &name.node.name,
                    None => "",
                };

                if self.structs.contains_key(name) && !name.is_empty() {
                    Some(self.structs.get(name).unwrap().clone())
                } else {
                    let mut fs: Vec<(String, Ty)> = Vec::new();
                    if let Some(decls) = declarations {
                        for d in decls.iter() {
                            match &d.node {
                                StructDeclaration::Field(f) => {
                                    let mut base_field_type =
                                        self.s_type_(&f.node.specifiers).unwrap();
                                    for struct_decl in f.node.declarators.iter() {
                                        let name = name_from_decl(
                                            &struct_decl.node.declarator.clone().unwrap().node,
                                        );
                                        let decl =
                                            struct_decl.node.declarator.clone().unwrap().node;
                                        let derived_ty = self
                                            .get_derived_type(&mut base_field_type, &decl.derived);
                                        fs.push((name, derived_ty.clone()));
                                    }
                                }
                                StructDeclaration::StaticAssert(_a) => {
                                    unimplemented!("Struct static assert not implemented!");
                                }
                            }
                        }
                    }
                    let s_ty = Ty::Struct(name.to_string(), FieldList::new(fs));
                    if !name.is_empty() {
                        self.structs.insert(name.to_string(), s_ty.clone());
                    }
                    Some(s_ty)
                }
            }
            _ => unimplemented!("Type {:#?} not implemented yet.", t),
        };
    }

    fn get_inner_derived_type(&mut self, base_ty: &Ty, d: &DerivedDeclarator) -> Ty {
        match &d {
            DerivedDeclarator::Array(arr) => {
                if let ArraySize::VariableExpression(expr) = &arr.node.size {
                    let expr_ = self.gen_expr(&expr.node);
                    let size = self.fold_(&expr_) as usize;

                    // flatten array here
                    return match base_ty {
                        Ty::Array(s, sizes, t) => {
                            let new_size = size * s;
                            let mut new_sizes = sizes.clone();
                            new_sizes.push(size);
                            Ty::Array(new_size, new_sizes.to_vec(), Box::new(*t.clone()))
                        }
                        _ => {
                            let sizes: Vec<usize> = vec![size];
                            Ty::Array(size, sizes, Box::new(base_ty.clone()))
                        }
                    };
                }
                panic!("Derived type not array");
            }
            DerivedDeclarator::Pointer(_) => {
                // let num_bits = base_ty.num_bits();
                // TODO: assume 32 bit ptrs for now.
                Ty::Ptr(32, Box::new(base_ty.clone()))
            }
            _ => panic!("Not implemented: {:#?}", d),
        }
    }

    // Function for getting base_type of an object (array, struct)
    pub fn get_derived_type(
        &mut self,
        base_ty: &mut Ty,
        derived: &[Node<DerivedDeclarator>],
    ) -> Ty {
        if derived.is_empty() {
            return base_ty.clone();
        }
        let derived_ty = base_ty;
        for d in derived.iter().rev() {
            *derived_ty = self.get_inner_derived_type(derived_ty, &d.node);
        }
        derived_ty.clone()
    }

    pub fn get_decl_info(&mut self, decl: &Declaration) -> Vec<DeclInfo> {
        let mut ty: Ty = self.d_type_(&decl.specifiers).unwrap();
        for d in decl.declarators.iter() {
            let derived = &d.node.declarator.node.derived;
            let derived_ty = self.get_derived_type(&mut ty, &derived.to_vec());
            ty = derived_ty;
        }

        let mut res: Vec<DeclInfo> = Vec::new();
        for node in decl.declarators.iter() {
            let decl_name = name_from_decl(&node.node.declarator.node);

            // add to structs
            let ty = match &ty {
                Ty::Struct(_, _) => {
                    if !self.structs.contains_key(&decl_name) {
                        self.structs.insert(decl_name.clone(), ty.clone());
                        ty.clone()
                    } else {
                        self.structs.get(&decl_name).unwrap().clone()
                    }
                }
                _ => ty.clone(),
            };

            let d = DeclInfo {
                name: decl_name,
                ty: ty.clone(),
            };
            res.push(d);
        }
        res
    }

    pub fn get_param_info(&mut self, decl: &ParameterDeclaration, v: bool) -> ParamInfo {
        let mut vis: Option<PartyId> = None;
        let base_ty: Option<Ty>;
        if v {
            vis = interpret_visibility(&decl.specifiers[0].node, self.mode);
            base_ty = self.d_type_(&decl.specifiers[1..]);
        } else {
            base_ty = self.d_type_(&decl.specifiers);
        }
        let d = &decl.declarator.as_ref().unwrap().node;
        let derived_ty = self.get_derived_type(&mut base_ty.unwrap(), &d.derived.to_vec());
        let name = name_from_decl(d);

        ParamInfo {
            name,
            ty: derived_ty,
            vis,
        }
    }

    pub fn get_fn_info(&mut self, fn_def: &FunctionDefinition) -> FnInfo {
        let name = name_from_func(fn_def);
        let ret_ty = self.ret_ty_from_func(fn_def);
        let args = match args_from_func(fn_def) {
            Some(args) => args.to_vec(),
            None => vec![],
        };
        let body = body_from_func(fn_def);

        FnInfo {
            name,
            ret_ty,
            args,
            body,
        }
    }

    fn ret_ty_from_func(&mut self, fn_def: &FunctionDefinition) -> Option<Ty> {
        self.d_type_(&fn_def.specifiers)
    }

    pub fn field_select(&self, struct_: &CTerm, field: &str) -> Result<CTerm, String> {
        if let CTermData::CStruct(_, fs) = &struct_.term {
            if let Some((_, term_)) = fs.search(field) {
                Ok(term_.clone())
            } else {
                Err(format!("No field '{}'", field))
            }
        } else {
            Err(format!("{} is not a struct", struct_))
        }
    }

    pub fn field_store(&self, struct_: &CTerm, field: &str, val: &CTerm) -> Result<CTerm, String> {
        if let CTermData::CStruct(struct_ty, fs) = &struct_.term {
            if let Some((idx, _)) = fs.search(field) {
                let mut new_fs = fs.clone();
                new_fs.set(idx, val.clone());
                let res = cterm(CTermData::CStruct(struct_ty.clone(), new_fs.clone()));
                Ok(res)
            } else {
                Err(format!("No field '{}'", field))
            }
        } else {
            Err(format!("{} is not a struct", struct_))
        }
    }

    fn array_select(&self, array: &CTerm, idx: &CTerm) -> Result<CTerm, String> {
        match (array.clone().term, idx.clone().term) {
            (CTermData::CArray(ty, id), CTermData::CInt(_, _, idx)) => {
                let i = id.unwrap_or_else(|| panic!("Unknown AllocID: {:#?}", array));
                let inner_ty = ty.inner_ty();
                Ok(cterm(match inner_ty {
                    Ty::Bool => CTermData::CBool(self.circ.load(i, idx)),
                    Ty::Int(s, w) => CTermData::CInt(s, w, self.circ.load(i, idx)),
                    _ => unimplemented!(),
                }))
            }
            (CTermData::CStackPtr(ty, offset, id), CTermData::CInt(_, _, idx)) => {
                let i = id.unwrap_or_else(|| panic!("Unknown AllocID: {:#?}", array));
                let inner_ty = ty.inner_ty();
                let new_offset = term![BV_ADD; offset, idx];
                Ok(cterm(match inner_ty {
                    Ty::Bool => CTermData::CBool(self.circ.load(i, new_offset)),
                    Ty::Int(s, w) => CTermData::CInt(s, w, self.circ.load(i, new_offset)),
                    _ => unimplemented!(),
                }))
            }
            (a, b) => Err(format!("[Array Select] cannot index {} by {}", a, b)),
        }
    }

    pub fn array_store(
        &mut self,
        array: &CTerm,
        idx: &CTerm,
        val: &CTerm,
    ) -> Result<CTerm, String> {
        match (array.clone().term, idx.clone().term) {
            (CTermData::CArray(ty, id), CTermData::CInt(_, _, idx_term)) => {
                let i = id.unwrap_or_else(|| panic!("Unknown AllocID: {:#?}", array.clone()));
                let vals = val.term.terms(self.circ.cir_ctx());
                for (o, v) in vals.iter().enumerate() {
                    let updated_idx = term![BV_ADD; idx_term.clone(), bv_lit(o as i32, 32)];
                    self.circ.store(i, updated_idx, v.clone());
                }
                if vals.len() > 1 {
                    Ok(cterm(CTermData::CArray(ty, id)))
                } else {
                    Ok(val.clone())
                }
            }
            (CTermData::CStackPtr(ty, offset, id), CTermData::CInt(_, _, idx_term)) => {
                let i = id.unwrap_or_else(|| panic!("Unknown AllocID: {:#?}", array.clone()));
                let vals = val.term.terms(self.circ.cir_ctx());
                for (o, v) in vals.iter().enumerate() {
                    let updated_idx =
                        term![BV_ADD; idx_term.clone(), offset.clone(), bv_lit(o as i32, 32)];
                    self.circ.store(i, updated_idx, v.clone());
                }
                if vals.len() > 1 {
                    Ok(cterm(CTermData::CArray(ty, id)))
                } else {
                    Ok(val.clone())
                }
            }
            (a, b) => Err(format!("[Array Store] cannot index {} by {}", a, b)),
        }
    }

    /// Computes base[val / loc]    
    fn rebuild_lval(&mut self, base: CTerm, loc: CLoc, val: CTerm) -> Result<CTerm, String> {
        match loc {
            CLoc::Var(_) => Ok(val),
            CLoc::Idx(inner_loc, idx) => {
                let old_inner = self.array_select(&base, &idx)?;
                let new_inner = self.rebuild_lval(old_inner, *inner_loc, val)?;
                self.array_store(&base, &idx, &new_inner)
            }
            CLoc::Member(inner_loc, field) => {
                let old_inner = self.field_select(&base, &field)?;
                let new_inner = self.rebuild_lval(old_inner, *inner_loc, val)?;
                self.field_store(&base, &field, &new_inner)
            }
        }
    }

    fn base_loc(&self, loc: CLoc) -> CLoc {
        match loc {
            CLoc::Var(_) => loc,
            CLoc::Member(l, _) => self.base_loc(*l),
            CLoc::Idx(l, _) => self.base_loc(*l),
        }
    }

    fn gen_lval(&mut self, expr: &Expression) -> CLoc {
        match &expr {
            Expression::Identifier(_) => {
                let base_name = name_from_expr(expr);
                CLoc::Var(Loc::local(base_name))
            }
            Expression::BinaryOperator(ref node) => {
                let bin_op = &node.node;
                match bin_op.operator.node {
                    BinaryOperator::Index => {
                        // get location
                        let loc = self.gen_lval(&bin_op.lhs.node);

                        // get offset
                        let index = self.gen_index(expr);
                        let offset = self.index_offset(&index);
                        let idx = cterm(CTermData::CInt(true, 32, offset));

                        if let Expression::BinaryOperator(_) = bin_op.lhs.node {
                            // Matrix case
                            let base = self.base_loc(loc);
                            CLoc::Idx(Box::new(base), idx)
                        } else {
                            CLoc::Idx(Box::new(loc), idx)
                        }
                    }
                    _ => unimplemented!("Invalid left hand value"),
                }
            }
            Expression::Member(node) => {
                let MemberExpression {
                    operator: _operator,
                    expression,
                    identifier,
                } = &node.node;
                let base_name = name_from_expr(&expression.node);
                let field_name = &identifier.node.name;
                CLoc::Member(
                    Box::new(CLoc::Var(Loc::local(base_name))),
                    field_name.to_string(),
                )
            }
            _ => unimplemented!("Invalid left hand value"),
        }
    }

    fn gen_assign(&mut self, loc: CLoc, val: CTerm) -> Result<CTerm, String> {
        match loc {
            CLoc::Var(l) => Ok(self
                .circ
                .assign(l, Val::Term(val))
                .map_err(|e| format!("{}", e))?
                .unwrap_term()),
            CLoc::Idx(l, idx) => {
                let old_inner: CTerm = match *l {
                    CLoc::Var(inner_loc) => self
                        .circ
                        .get_value(inner_loc)
                        .map_err(|e| format!("{}", e))?
                        .unwrap_term(),
                    CLoc::Member(inner_loc, field) => {
                        let base = self
                            .circ
                            .get_value(inner_loc.loc().clone())
                            .map_err(|e| format!("{}", e))?
                            .unwrap_term();
                        self.field_select(&base, &field).unwrap()
                    }
                    CLoc::Idx(inner_loc, idx) => {
                        let base = self
                            .circ
                            .get_value(inner_loc.loc().clone())
                            .map_err(|e| format!("{}", e))?
                            .unwrap_term();
                        self.array_select(&base, &idx).unwrap()
                    }
                };
                self.array_store(&old_inner, &idx, &val)
            }
            CLoc::Member(l, field) => {
                let inner_loc = l.loc().clone();
                let base = self
                    .circ
                    .get_value(inner_loc.clone())
                    .map_err(|e| format!("{}", e))?
                    .unwrap_term();
                let old_inner = self.field_select(&base, &field)?;
                let new_inner = self.rebuild_lval(old_inner, *l, val)?;
                let res = self.field_store(&base, &field, &new_inner);
                Ok(self
                    .circ
                    .assign(inner_loc, Val::Term(res.unwrap()))
                    .map_err(|e| format!("{}", e))?
                    .unwrap_term())
            }
        }
    }

    fn fold_(&mut self, expr: &CTerm) -> i32 {
        let term_ = fold(&expr.term.term(self.circ.cir_ctx()), &[]);
        let cterm_ = cterm(CTermData::CInt(true, 32, term_));
        let val = const_int(cterm_);
        val.to_i32().unwrap()
    }

    fn const_(&self, c: &Constant) -> CTerm {
        match c {
            // TODO: move const integer function out to separate function
            Constant::Integer(i) => {
                let signed = !i.suffix.unsigned;
                let _imaginary = i.suffix.imaginary;
                match (i.suffix.size, signed) {
                    (IntegerSize::Int, true) => {
                        let size = 32;
                        let num = i.number.parse::<i32>().unwrap();
                        cterm(CTermData::CInt(signed, size, bv_lit(num, size)))
                    }
                    (IntegerSize::Int, false) => {
                        let size = 32;
                        let num = i.number.parse::<u32>().unwrap();
                        cterm(CTermData::CInt(signed, size, bv_lit(num, size)))
                    }
                    (IntegerSize::Long, true) => {
                        let size = 64;
                        let num = i.number.parse::<i64>().unwrap();
                        cterm(CTermData::CInt(signed, size, bv_lit(num, size)))
                    }
                    (IntegerSize::Long, false) => {
                        let size = 64;
                        let num = i.number.parse::<u64>().unwrap();
                        cterm(CTermData::CInt(signed, size, bv_lit(num, size)))
                    }
                    _ => unimplemented!("Unimplemented constant literal: {:?}", i),
                }
            }
            _ => unimplemented!("Constant {:#?} hasn't been implemented", c),
        }
    }

    fn get_bin_op(&self, op: &BinaryOperator) -> fn(CTerm, CTerm) -> Result<CTerm, String> {
        match &op {
            BinaryOperator::Plus => add,
            BinaryOperator::AssignPlus => add,
            BinaryOperator::AssignDivide => div,
            BinaryOperator::Minus => sub,
            BinaryOperator::Multiply => mul,
            BinaryOperator::Divide => div,
            BinaryOperator::Equals => eq,
            BinaryOperator::NotEquals => neq,
            BinaryOperator::Greater => ugt,
            BinaryOperator::GreaterOrEqual => uge,
            BinaryOperator::Less => ult,
            BinaryOperator::LessOrEqual => ule,
            BinaryOperator::BitwiseAnd => bitand,
            BinaryOperator::BitwiseOr => bitor,
            BinaryOperator::BitwiseXor => bitxor,
            BinaryOperator::LogicalAnd => and,
            BinaryOperator::LogicalOr => or,
            BinaryOperator::Modulo => rem,
            BinaryOperator::ShiftLeft => shl,
            BinaryOperator::ShiftRight => shr,
            _ => unimplemented!("BinaryOperator {:#?} hasn't been implemented", op),
        }
    }

    fn get_u_op(&self, op: &UnaryOperator) -> fn(CTerm, CTerm) -> Result<CTerm, String> {
        match &op {
            UnaryOperator::PostIncrement => add,
            UnaryOperator::PostDecrement => sub,
            _ => unimplemented!("UnaryOperator {:#?} hasn't been implemented", op),
        }
    }

    fn gen_index(&mut self, expr: &Expression) -> IndexTerm {
        match &expr {
            Expression::BinaryOperator(node) => {
                let bin_op = &node.node;
                match bin_op.operator.node {
                    BinaryOperator::Index => {
                        let mut a = self.gen_index(&bin_op.lhs.node);
                        let b = self.gen_index(&bin_op.rhs.node);
                        a.indices.push(b.base);
                        a
                    }
                    _ => IndexTerm {
                        base: self.gen_expr(expr),
                        indices: Vec::new(),
                    },
                }
            }
            _ => IndexTerm {
                base: self.gen_expr(expr),
                indices: Vec::new(),
            },
        }
    }

    fn index_offset(&mut self, index: &IndexTerm) -> Term {
        let base_ty = index.base.term.type_();
        let mut offset: Term = bv_lit(0, 32);
        if let Ty::Array(_, sizes, _) = base_ty {
            let mut total = 1;
            for (i, ind) in index.indices.iter().rev().enumerate() {
                let index_term = ind.term.term(self.circ.cir_ctx());
                let size = sizes[i] as i32;
                if i == 0 {
                    offset = term![BV_ADD; index_term, offset];
                } else {
                    offset = term![BV_ADD; term![BV_MUL; bv_lit(total, 32), index_term], offset];
                }
                total *= size;
            }
        } else {
            assert!(index.indices.len() == 1);
            let index_term = index.indices[0].term.term(self.circ.cir_ctx());
            offset = index_term;
        }
        offset
    }

    fn gen_expr(&mut self, expr: &Expression) -> CTerm {
        let res = match &expr {
            Expression::Identifier(node) => Ok(self
                .unwrap(self.circ.get_value(Loc::local(node.node.name.clone())))
                .unwrap_term()),
            Expression::Constant(node) => Ok(self.const_(&node.node)),
            Expression::BinaryOperator(node) => {
                let bin_op = &node.node;
                match bin_op.operator.node {
                    BinaryOperator::Assign => {
                        let loc = self.gen_lval(&bin_op.lhs.node);
                        let val = self.gen_expr(&bin_op.rhs.node);
                        self.gen_assign(loc, val)
                    }
                    BinaryOperator::AssignPlus | BinaryOperator::AssignDivide => {
                        let f = self.get_bin_op(&bin_op.operator.node);
                        let i = self.gen_expr(&bin_op.lhs.node);
                        let rhs = self.gen_expr(&bin_op.rhs.node);
                        let loc = self.gen_lval(&bin_op.lhs.node);
                        let val = f(i, rhs).unwrap();
                        self.gen_assign(loc, val)
                    }
                    BinaryOperator::Index => {
                        let index = self.gen_index(expr);
                        let offset = self.index_offset(&index);
                        match index.base.term {
                            CTermData::CArray(ref ty, id) => {
                                // TODO: please clean this
                                if let Ty::Array(size, sizes, t) = ty {
                                    if index.indices.len() < sizes.len() {
                                        let diff = sizes.len() - index.indices.len();
                                        let new_sizes: Vec<usize> =
                                            sizes.clone().into_iter().take(diff).collect();

                                        let new_ty =
                                            Ty::Array(*size, new_sizes, Box::new(*t.clone()));
                                        Ok(cterm(CTermData::CStackPtr(new_ty, offset, id)))
                                    } else {
                                        self.array_select(
                                            &index.base,
                                            &cterm(CTermData::CInt(true, 32, offset)),
                                        )
                                    }
                                } else {
                                    self.array_select(
                                        &index.base,
                                        &cterm(CTermData::CInt(true, 32, offset)),
                                    )
                                }
                            }
                            _ => self.array_select(
                                &index.base,
                                &cterm(CTermData::CInt(true, 32, offset)),
                            ),
                        }
                    }
                    _ => {
                        let f = self.get_bin_op(&bin_op.operator.node);
                        let a = self.gen_expr(&bin_op.lhs.node);
                        let mut b = self.gen_expr(&bin_op.rhs.node);

                        // TODO: fix hack, const int check for shifting
                        match bin_op.operator.node {
                            BinaryOperator::ShiftLeft | BinaryOperator::ShiftRight => {
                                let b_t = fold(&b.term.term(self.circ.cir_ctx()), &[]);
                                b = cterm(CTermData::CInt(true, 32, b_t));
                                f(a, b)
                            }
                            _ => f(a, b),
                        }
                    }
                }
            }
            Expression::UnaryOperator(node) => {
                let u_op = &node.node;
                match u_op.operator.node {
                    UnaryOperator::PostIncrement | UnaryOperator::PostDecrement => {
                        let f = self.get_u_op(&u_op.operator.node);
                        let i = self.gen_expr(&u_op.operand.node);
                        let one = cterm(CTermData::CInt(true, 32, bv_lit(1, 32)));
                        let loc = self.gen_lval(&u_op.operand.node);
                        let val = f(i, one).unwrap();
                        self.gen_assign(loc, val)
                    }
                    UnaryOperator::SizeOf => {
                        let ty = match &u_op.operand.node {
                            Expression::Identifier(name) => {
                                let n = &name.node.name;
                                match self.typedefs.get(n) {
                                    Some(ty) => ty.clone(),
                                    None => panic!("Unknown type: {}", n),
                                }
                            }
                            _ => unimplemented!("Unimplemented Sizeof: {:#?}", u_op.operand.node),
                        };
                        let _size = ty.num_bits();
                        Ok(cterm(CTermData::CInt(true, 32, bv_lit(1, 32))))
                    }
                    _ => unimplemented!("UnaryOperator {:#?} hasn't been implemented", u_op),
                }
            }
            Expression::Cast(node) => {
                let CastExpression {
                    type_name,
                    expression,
                } = &node.node;
                let to_ty = self.s_type_(&type_name.node.specifiers);
                let expr = self.gen_expr(&expression.node);
                Ok(cast(to_ty, expr))
            }
            Expression::Call(node) => {
                let CallExpression { callee, arguments } = &node.node;
                let fname = name_from_expr(&callee.node);

                let f = self
                    .functions
                    .get(&fname)
                    .unwrap_or_else(|| panic!("No function '{}'", fname))
                    .clone();

                let FnInfo {
                    name,
                    ret_ty,
                    args: parameters,
                    body,
                } = f;

                // Add parameters
                let args = arguments
                    .iter()
                    .map(|e| self.gen_expr(&e.node))
                    .collect::<Vec<_>>();

                // setup stack frame for entry function
                self.circ.enter_fn(name, ret_ty.clone());
                assert_eq!(parameters.len(), args.len());

                for (p, a) in parameters.iter().zip(args) {
                    let param_info = self.get_param_info(p, false);
                    let r = self.circ.declare_init(
                        param_info.name.clone(),
                        param_info.ty,
                        Val::Term(a),
                    );
                    self.unwrap(r);
                }

                self.gen_stmt(&body);

                let ret = self
                    .circ
                    .exit_fn()
                    .map(|a| a.unwrap_term())
                    .unwrap_or_else(|| cterm(CTermData::CBool(bool_lit(false))));

                match ret_ty {
                    None => Ok(ret),
                    _ => Ok(cast(ret_ty, ret)),
                }
            }
            Expression::Member(member) => {
                let MemberExpression {
                    operator: _operator,
                    expression,
                    identifier,
                } = &member.node;
                let base = self.gen_expr(&expression.node);
                let field = &identifier.node.name;
                self.field_select(&base, field)
            }
            Expression::SizeOf(s) => {
                let ty = self.s_type_(&s.node.specifiers);
                match ty {
                    Some(t) => {
                        let _size = t.num_bits();
                        Ok(cterm(CTermData::CInt(true, 32, bv_lit(1, 32))))
                    }
                    None => {
                        panic!("Cannot determine size of type: {:#?}", s);
                    }
                }
            }
            _ => unimplemented!("Expr {:#?} hasn't been implemented", expr),
        };
        self.unwrap(res)
    }

    fn gen_init(&mut self, ty: &Ty, init: &Initializer) -> CTerm {
        match init {
            Initializer::Expression(e) => self.gen_expr(&e.node),
            Initializer::List(ref l) => match ty.clone() {
                Ty::Array(n, _, _) => {
                    let mut values: Vec<CTerm> = Vec::new();
                    let inner_type = ty.clone().inner_ty();
                    let flattened_inits = flatten_inits(init);
                    for li in flattened_inits {
                        let expr = self.gen_init(&inner_type, li);
                        values.push(expr);
                    }
                    assert!(n == values.len());
                    let id = self
                        .circ
                        .zero_allocate(values.len(), 32, inner_type.num_bits());

                    for (i, v) in values.iter().enumerate() {
                        let offset = bv_lit(i, 32);
                        let v_ = v.term.term(self.circ.cir_ctx());
                        self.circ.store(id, offset, v_);
                    }
                    cterm(CTermData::CArray(ty.clone(), Some(id)))
                }
                Ty::Struct(_base, fs) => {
                    assert!(fs.len() == l.len());
                    ty.default(self.circ.cir_ctx())
                }
                _ => unreachable!("Initializer list for non-list type: {:#?}", l),
            },
        }
    }

    fn gen_decl(&mut self, decl: &Declaration) -> Vec<CTerm> {
        let specs = decl.specifiers.clone();
        if let DeclarationSpecifier::StorageClass(_store_node) = &specs[0].node {
            let new_decl = Declaration {
                specifiers: decl.specifiers[1..].to_vec(),
                declarators: decl.declarators.clone(),
            };
            let decl_infos = self.get_decl_info(&new_decl);
            for info in decl_infos.iter() {
                if !self.typedefs.contains_key(&info.name) {
                    self.typedefs.insert(info.name.clone(), info.ty.clone());
                } else {
                    panic!("Typedef already defined for: {}", info.name);
                }
            }
            Vec::new()
        } else {
            let decl_infos = self.get_decl_info(decl);
            let mut exprs: Vec<CTerm> = Vec::new();
            for (d, info) in decl.declarators.iter().zip(decl_infos.iter()) {
                let expr: CTerm = if let Some(init) = d.node.initializer.clone() {
                    self.gen_init(&info.ty, &init.node)
                } else {
                    info.ty.default(self.circ.cir_ctx())
                };

                let res = self.circ.declare_init(
                    info.name.clone(),
                    info.ty.clone(),
                    Val::Term(cast(Some(info.ty.clone()), expr.clone())),
                );
                self.unwrap(res);
                exprs.push(expr);
            }
            exprs
        }
    }

    //TODO: This function is not quite right because the loop body could modify the iteration variable.
    fn get_const_iters(&mut self, for_stmt: &ForStatement) -> ConstIteration {
        let init: Option<ConstIteration> = match &for_stmt.initializer.node {
            ForInitializer::Declaration(d) => {
                // TODO: need to identify which is the looping variable
                let exprs = self.gen_decl(&d.node);
                assert!(exprs.len() == 1);
                let val = self.fold_(&exprs[0]);
                Some(ConstIteration { val })
            }
            ForInitializer::Expression(e) => {
                if let Expression::BinaryOperator(_) = e.node {
                    let expr = self.gen_expr(&e.node);
                    let val = self.fold_(&expr);
                    Some(ConstIteration { val })
                } else {
                    None
                }
            }
            _ => None,
        };

        let cond: Option<ConstIteration> = match &for_stmt.condition.as_ref().unwrap().node {
            Expression::BinaryOperator(bin_op) => {
                let expr = self.gen_expr(&bin_op.node.rhs.node);
                let val = self.fold_(&expr);
                match bin_op.node.operator.node {
                    BinaryOperator::Less => Some(ConstIteration { val }),
                    BinaryOperator::LessOrEqual => Some(ConstIteration { val: val + 1 }),
                    BinaryOperator::Greater => Some(ConstIteration { val }),
                    BinaryOperator::GreaterOrEqual => Some(ConstIteration { val: val - 1 }),
                    _ => None,
                }
            }
            _ => None,
        };

        let step: Option<ConstIteration> = match &for_stmt.step.as_ref().unwrap().node {
            Expression::UnaryOperator(u_op) => match u_op.node.operator.node {
                UnaryOperator::PostIncrement | UnaryOperator::PreIncrement => {
                    Some(ConstIteration { val: 1 })
                }
                UnaryOperator::PostDecrement | UnaryOperator::PreDecrement => {
                    Some(ConstIteration { val: -1 })
                }
                _ => None,
            },
            Expression::BinaryOperator(bin_op) => match bin_op.node.operator.node {
                BinaryOperator::AssignPlus => {
                    let expr = self.gen_expr(&bin_op.node.rhs.node);
                    let val = self.fold_(&expr);
                    Some(ConstIteration { val })
                }
                _ => None,
            },
            _ => None,
        };

        // TODO: error checking here
        let init_ = init.unwrap();
        let cond_ = cond.unwrap();
        let incr_ = step.unwrap();

        let start: f32 = init_.val as f32;
        let end: f32 = cond_.val as f32;
        let incr: f32 = incr_.val as f32;

        ConstIteration {
            val: ((end - start) / incr).ceil() as i32,
        }
    }

    fn gen_stmt(&mut self, stmt: &Statement) {
        match stmt {
            Statement::Compound(nodes) => {
                for node in nodes {
                    match &node.node {
                        BlockItem::Declaration(decl) => {
                            self.gen_decl(&decl.node);
                        }
                        BlockItem::Statement(stmt) => {
                            self.gen_stmt(&stmt.node);
                        }
                        BlockItem::StaticAssert(_sa) => {
                            unimplemented!("Static Assert not supported yet")
                        }
                    }
                }
            }
            Statement::If(node) => {
                let cond = self.gen_expr(&node.node.condition.node);
                let t_res = self
                    .circ
                    .enter_condition(cond.term.term(self.circ.cir_ctx()));
                self.unwrap(t_res);
                self.gen_stmt(&node.node.then_statement.node);
                self.circ.exit_condition();
                if let Some(f_cond) = &node.node.else_statement {
                    let f_res = self
                        .circ
                        .enter_condition(term!(Op::Not; cond.term.term(self.circ.cir_ctx())));
                    self.unwrap(f_res);
                    self.gen_stmt(&f_cond.node);
                    self.circ.exit_condition();
                }
            }
            Statement::Return(ret) => {
                match ret {
                    Some(expr) => {
                        let ret = self.gen_expr(&expr.node);
                        let ret_res = self.circ.return_(Some(ret));
                        self.unwrap(ret_res);
                    }
                    None => {
                        let ret_res = self.circ.return_(None);
                        self.unwrap(ret_res);
                    }
                };
            }
            Statement::Expression(expr) => match expr {
                Some(e) => {
                    self.gen_expr(&e.node);
                }
                None => {}
            },
            Statement::For(for_stmt) => {
                // TODO: Add enter_breakable
                self.circ.enter_scope();
                let const_iters = self.get_const_iters(&for_stmt.node);
                // TODO: Loop 5 times if const not specified
                let bound = const_iters.val;

                for _ in 0..bound {
                    self.circ.enter_scope();
                    self.gen_stmt(&for_stmt.node.statement.node);
                    self.circ.exit_scope();
                    self.gen_expr(&for_stmt.node.step.as_ref().unwrap().node);
                }
                self.circ.exit_scope();
            }
            // TODO: add while loop
            _ => unimplemented!("Statement {:#?} hasn't been implemented", stmt),
        }
    }

    fn entry_fn(&mut self, n: &str) {
        debug!("Entry: {}", n);
        // find the entry function
        let f = self
            .functions
            .get(n)
            .unwrap_or_else(|| panic!("No function '{}'", n))
            .clone();

        // setup stack frame for entry function
        self.circ.enter_fn(f.name.to_owned(), f.ret_ty.clone());

        for arg in f.args.iter() {
            let param_info = self.get_param_info(arg, true);
            let r = self.circ.declare_input(
                param_info.name.clone(),
                &param_info.ty,
                param_info.vis,
                None,
                true,
            );
            self.unwrap(r);
        }

        self.gen_stmt(&f.body);

        if let Some(r) = self.circ.exit_fn() {
            match self.mode {
                Mode::Mpc(_) => {
                    let ret_term = r.unwrap_term();
                    let ret_terms = ret_term.term.terms(self.circ.cir_ctx());
                    self.circ
                        .cir_ctx()
                        .cs
                        .borrow_mut()
                        .outputs
                        .extend(ret_terms);
                }
                Mode::Proof => {
                    let ty = f.ret_ty.as_ref().unwrap();
                    let name = "return".to_owned();
                    let term = r.unwrap_term();
                    let r2 = self
                        .circ
                        .declare_input(name, ty, PROVER_VIS, None, false)
                        .unwrap();
                    self.circ.assert(eq(term, r2).unwrap().term.simple_term());
                    unimplemented!();
                }
                _ => unimplemented!("Mode: {}", self.mode),
            }
        }
    }

    fn visit_files(&mut self) {
        let TranslationUnit(nodes) = self.tu.clone();
        for n in nodes.iter() {
            match &n.node {
                ExternalDeclaration::Declaration(decl) => {
                    self.gen_decl(&decl.node);
                }
                ExternalDeclaration::FunctionDefinition(ref fn_def) => {
                    let fn_info = self.get_fn_info(&fn_def.node);
                    let fname = fn_info.name.clone();
                    self.functions.insert(fname, fn_info);
                }
                _ => unimplemented!("Haven't implemented node: {:?}", n.node),
            };
        }
    }
}
