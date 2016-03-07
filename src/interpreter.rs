// TODO(tsion): Remove this.
#![allow(unused_imports, dead_code, unused_variables)]

use byteorder::{self, ByteOrder};
use rustc::middle::const_eval;
use rustc::middle::cstore::CrateStore;
use rustc::middle::def_id;
use rustc::middle::ty::{self, TyCtxt};
use rustc::mir::mir_map::MirMap;
use rustc::mir::repr::{self as mir, Mir};
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::iter;
use syntax::ast::Attribute;
use syntax::attr::AttrMetaMethods;

use memory::{self, Pointer, Repr, Allocation};

const TRACE_EXECUTION: bool = true;

#[derive(Clone, Debug)]
pub enum EvalError {
    DanglingPointerDeref,
    InvalidBool,
    PointerOutOfBounds,
}

pub type EvalResult<T> = Result<T, EvalError>;

impl Error for EvalError {
    fn description(&self) -> &str {
        match *self {
            EvalError::DanglingPointerDeref => "dangling pointer was dereferenced",
            EvalError::InvalidBool => "invalid boolean value read",
            EvalError::PointerOutOfBounds => "pointer offset outside bounds of allocation",
        }
    }

    fn cause(&self) -> Option<&Error> { None }
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.description())
    }
}

// #[derive(Clone, Debug, PartialEq)]
// enum Value {
//     Uninit,
//     Bool(bool),
//     Int(i64), // FIXME(tsion): Should be bit-width aware.
//     Pointer(Pointer),
//     Adt { variant: usize, data_ptr: Pointer },
//     Func(def_id::DefId),
// }

/// A stack frame.
#[derive(Debug)]
struct Frame {
    /// A pointer for writing the return value of the current call, if it's not a diverging call.
    return_ptr: Option<Pointer>,

    /// The list of locals for the current function, stored in order as
    /// `[arguments..., variables..., temporaries...]`. The variables begin at `self.var_offset`
    /// and the temporaries at `self.temp_offset`.
    locals: Vec<Pointer>,

    /// The offset of the first variable in `self.locals`.
    var_offset: usize,

    /// The offset of the first temporary in `self.locals`.
    temp_offset: usize,
}

impl Frame {
    fn arg_ptr(&self, i: u32) -> Pointer {
        self.locals[i as usize].clone()
    }

    fn var_ptr(&self, i: u32) -> Pointer {
        self.locals[self.var_offset + i as usize].clone()
    }

    fn temp_ptr(&self, i: u32) -> Pointer {
        self.locals[self.temp_offset + i as usize].clone()
    }
}

struct Interpreter<'a, 'tcx: 'a> {
    tcx: &'a TyCtxt<'tcx>,
    mir_map: &'a MirMap<'tcx>,
    memory: memory::Memory,
    stack: Vec<Frame>,
}

impl<'a, 'tcx> Interpreter<'a, 'tcx> {
    fn new(tcx: &'a TyCtxt<'tcx>, mir_map: &'a MirMap<'tcx>) -> Self {
        Interpreter {
            tcx: tcx,
            mir_map: mir_map,
            memory: memory::Memory::new(),
            stack: Vec::new(),
        }
    }

    fn push_stack_frame(&mut self, mir: &Mir, args: &[&mir::Operand], return_ptr: Option<Pointer>)
        -> EvalResult<()>
    {
        let num_args = mir.arg_decls.len();
        let num_vars = mir.var_decls.len();
        assert_eq!(args.len(), num_args);

        let arg_tys = mir.arg_decls.iter().map(|a| a.ty);
        let var_tys = mir.var_decls.iter().map(|v| v.ty);
        let temp_tys = mir.temp_decls.iter().map(|t| t.ty);

        let locals: Vec<Pointer> = arg_tys.chain(var_tys).chain(temp_tys).map(|ty| {
            self.memory.allocate(Repr::from_ty(ty))
        }).collect();

        for (dest, operand) in locals[..num_args].iter().zip(args) {
            let src = try!(self.operand_to_ptr(operand));
            try!(self.memory.copy(&src, dest, dest.repr.size()));
        }

        self.stack.push(Frame { 
            return_ptr: return_ptr,
            locals: locals,
            var_offset: num_args,
            temp_offset: num_args + num_vars,
        });

        Ok(())
    }

    fn pop_stack_frame(&mut self) {
        let _frame = self.stack.pop().expect("tried to pop a stack frame, but there were none");
        // TODO(tsion): Deallocate local variables.
    }

    fn call(&mut self, mir: &Mir, args: &[&mir::Operand], return_ptr: Option<Pointer>)
        -> EvalResult<()>
    {
        try!(self.push_stack_frame(mir, args, return_ptr));
        let mut current_block = mir::START_BLOCK;

        loop {
            if TRACE_EXECUTION { println!("Entering block: {:?}", current_block); }
            let block_data = mir.basic_block_data(current_block);

            for stmt in &block_data.statements {
                if TRACE_EXECUTION { println!("{:?}", stmt); }
                let mir::StatementKind::Assign(ref lvalue, ref rvalue) = stmt.kind;
                let ptr = try!(self.lvalue_to_ptr(lvalue));
                try!(self.eval_rvalue_into(rvalue, &ptr));
            }

            if TRACE_EXECUTION { println!("{:?}", block_data.terminator()); }

            use rustc::mir::repr::Terminator::*;
            match *block_data.terminator() {
                Return => break,

                Goto { target } => current_block = target,

                If { ref cond, targets: (then_target, else_target) } => {
                    let cond_ptr = try!(self.operand_to_ptr(cond));
                    let cond = try!(self.memory.read_bool(&cond_ptr));
                    current_block = if cond { then_target } else { else_target };
                }

                // Call { ref func, ref args, ref destination, .. } => {
                //     let ptr = destination.as_ref().map(|&(ref lv, _)| self.lvalue_to_ptr(lv));
                //     let func_val = self.operand_to_ptr(func);

                //     if let Value::Func(def_id) = func_val {
                //         let mir_data;
                //         let mir = match self.tcx.map.as_local_node_id(def_id) {
                //             Some(node_id) => self.mir_map.map.get(&node_id).unwrap(),
                //             None => {
                //                 let cstore = &self.tcx.sess.cstore;
                //                 mir_data = cstore.maybe_get_item_mir(self.tcx, def_id).unwrap();
                //                 &mir_data
                //             }
                //         };

                //         let arg_vals: Vec<Value> =
                //             args.iter().map(|arg| self.operand_to_ptr(arg)).collect();

                //         self.call(mir, &arg_vals, ptr);

                //         if let Some((_, target)) = *destination {
                //             current_block = target;
                //         }
                //     } else {
                //         panic!("tried to call a non-function value: {:?}", func_val);
                //     }
                // }

                // SwitchInt { ref discr, ref values, ref targets, .. } => {
                //     let discr_val = self.read_lvalue(discr);

                //     let index = values.iter().position(|v| discr_val == self.const_to_ptr(v))
                //         .expect("discriminant matched no values");

                //     current_block = targets[index];
                // }

                // Switch { ref discr, ref targets, .. } => {
                //     let discr_val = self.read_lvalue(discr);

                //     if let Value::Adt { variant, .. } = discr_val {
                //         current_block = targets[variant];
                //     } else {
                //         panic!("Switch on non-Adt value: {:?}", discr_val);
                //     }
                // }

                Drop { target, .. } => {
                    // TODO: Handle destructors and dynamic drop.
                    current_block = target;
                }

                Resume => unimplemented!(),
                _ => unimplemented!(),
            }
        }

        self.pop_stack_frame();
        Ok(())
    }

    fn lvalue_to_ptr(&self, lvalue: &mir::Lvalue) -> EvalResult<Pointer> {
        let frame = self.stack.last().expect("no call frames exists");

        use rustc::mir::repr::Lvalue::*;
        let ptr = match *lvalue {
            ReturnPointer => frame.return_ptr.clone()
                .expect("ReturnPointer used in a function with no return value"),
            Arg(i)  => frame.arg_ptr(i),
            Var(i)  => frame.var_ptr(i),
            Temp(i) => frame.temp_ptr(i),
            ref l => panic!("can't handle lvalue: {:?}", l),
        };

        Ok(ptr)

        //     mir::Lvalue::Projection(ref proj) => {
        //         let base_ptr = self.lvalue_to_ptr(&proj.base);

        //         match proj.elem {
        //             mir::ProjectionElem::Field(field, _) => {
        //                 base_ptr.offset(field.index())
        //             }

        //             mir::ProjectionElem::Downcast(_, variant) => {
        //                 let adt_val = self.read_pointer(base_ptr);
        //                 if let Value::Adt { variant: actual_variant, data_ptr } = adt_val {
        //                     debug_assert_eq!(variant, actual_variant);
        //                     data_ptr
        //                 } else {
        //                     panic!("Downcast attempted on non-ADT: {:?}", adt_val)
        //                 }
        //             }

        //             mir::ProjectionElem::Deref => {
        //                 let ptr_val = self.read_pointer(base_ptr);
        //                 if let Value::Pointer(ptr) = ptr_val {
        //                     ptr
        //                 } else {
        //                     panic!("Deref attempted on non-pointer: {:?}", ptr_val)
        //                 }
        //             }

        //             mir::ProjectionElem::Index(ref _operand) => unimplemented!(),
        //             mir::ProjectionElem::ConstantIndex { .. } => unimplemented!(),
        //         }
        //     }

        //     _ => unimplemented!(),
        // }
    }

    fn eval_binary_op(&mut self, bin_op: mir::BinOp, left: Pointer, right: Pointer, dest: &Pointer)
            -> EvalResult<()> {
        use rustc::mir::repr::BinOp::*;
        match (&left.repr, &right.repr, &dest.repr) {
            (&Repr::Int, &Repr::Int, &Repr::Int) => {
                let l = try!(self.memory.read_int(&left));
                let r = try!(self.memory.read_int(&right));
                let n = match bin_op {
                    Add    => l + r,
                    Sub    => l - r,
                    Mul    => l * r,
                    Div    => l / r,
                    Rem    => l % r,
                    BitXor => l ^ r,
                    BitAnd => l & r,
                    BitOr  => l | r,
                    Shl    => l << r,
                    Shr    => l >> r,
                    _      => unimplemented!(),
                    // Eq     => Value::Bool(l == r),
                    // Lt     => Value::Bool(l < r),
                    // Le     => Value::Bool(l <= r),
                    // Ne     => Value::Bool(l != r),
                    // Ge     => Value::Bool(l >= r),
                    // Gt     => Value::Bool(l > r),
                };
                self.memory.write_int(dest, n)
            }
            (l, r, o) =>
                panic!("unhandled binary operation: {:?}({:?}, {:?}) into {:?}", bin_op, l, r, o),
        }
    }

    fn eval_rvalue_into(&mut self, rvalue: &mir::Rvalue, dest: &Pointer) -> EvalResult<()> {
        use rustc::mir::repr::Rvalue::*;
        match *rvalue {
            Use(ref operand) => {
                let src = try!(self.operand_to_ptr(operand));
                try!(self.memory.copy(&src, dest, src.repr.size()));
            }

            BinaryOp(bin_op, ref left, ref right) => {
                let left_ptr = try!(self.operand_to_ptr(left));
                let right_ptr = try!(self.operand_to_ptr(right));
                try!(self.eval_binary_op(bin_op, left_ptr, right_ptr, dest));
            }

            UnaryOp(un_op, ref operand) => {
                let ptr = try!(self.operand_to_ptr(operand));
                let m = try!(self.memory.read_int(&ptr));
                let n = match (un_op, ptr.repr) {
                    (mir::UnOp::Not, Repr::Int) => !m,
                    (mir::UnOp::Neg, Repr::Int) => -m,
                    (_, ref p) => panic!("unhandled binary operation: {:?}({:?})", un_op, p),
                };
                try!(self.memory.write_int(dest, n));
            }

            Aggregate(mir::AggregateKind::Tuple, ref operands) => {
                match dest.repr {
                    Repr::Aggregate { ref fields, .. } => {
                        for (field, operand) in fields.iter().zip(operands) {
                            let src = try!(self.operand_to_ptr(operand));
                            try!(self.memory.copy(&src, &dest.offset(field.offset), src.repr.size()));
                        }
                    }

                    _ => panic!("attempted to write tuple rvalue '{:?}' into non-aggregate pointer '{:?}'",
                                rvalue, dest)
                }
            }

            // Ref(_region, _kind, ref lvalue) => {
            //     Value::Pointer(self.lvalue_to_ptr(lvalue))
            // }

            // Aggregate(mir::AggregateKind::Adt(ref adt_def, variant, _substs),
            //                        ref operands) => {
            //     let max_fields = adt_def.variants
            //         .iter()
            //         .map(|v| v.fields.len())
            //         .max()
            //         .unwrap_or(0);

            //     let ptr = self.allocate_aggregate(max_fields);

            //     for (i, operand) in operands.iter().enumerate() {
            //         let val = self.operand_to_ptr(operand);
            //         self.write_pointer(ptr.offset(i), val);
            //     }

            //     Value::Adt { variant: variant, data_ptr: ptr }
            // }

            ref r => panic!("can't handle rvalue: {:?}", r),
        }

        Ok(())
    }

    fn operand_to_ptr(&mut self, op: &mir::Operand) -> EvalResult<Pointer> {
        use rustc::mir::repr::Operand::*;
        match *op {
            Consume(ref lvalue) => self.lvalue_to_ptr(lvalue),

            Constant(ref constant) => {
                use rustc::mir::repr::Literal::*;
                match constant.literal {
                    Value { ref value } => self.const_to_ptr(value),

                    Item { def_id, kind, .. } => match kind {
                        // mir::ItemKind::Function | mir::ItemKind::Method => Value::Func(def_id),
                        _ => panic!("can't handle item literal: {:?}", constant.literal),
                    },
                }
            }
        }
    }

    fn const_to_ptr(&mut self, const_val: &const_eval::ConstVal) -> EvalResult<Pointer> {
        use rustc::middle::const_eval::ConstVal::*;
        match *const_val {
            Float(_f) => unimplemented!(),
            Int(n) => {
                let ptr = self.memory.allocate(Repr::Int);
                try!(self.memory.write_int(&ptr, n));
                Ok(ptr)
            }
            Uint(_u)          => unimplemented!(),
            Str(ref _s)       => unimplemented!(),
            ByteStr(ref _bs)  => unimplemented!(),
            Bool(b) => {
                let ptr = self.memory.allocate(Repr::Bool);
                try!(self.memory.write_bool(&ptr, b));
                Ok(ptr)
            },
            Struct(_node_id)  => unimplemented!(),
            Tuple(_node_id)   => unimplemented!(),
            Function(_def_id) => unimplemented!(),
            Array(_, _)       => unimplemented!(),
            Repeat(_, _)      => unimplemented!(),
        }
    }
}

pub fn interpret_start_points<'tcx>(tcx: &TyCtxt<'tcx>, mir_map: &MirMap<'tcx>) {
    for (&id, mir) in &mir_map.map {
        for attr in tcx.map.attrs(id) {
            if attr.check_name("miri_run") {
                let item = tcx.map.expect_item(id);

                println!("Interpreting: {}", item.name);

                let mut miri = Interpreter::new(tcx, mir_map);
                let return_ptr = match mir.return_ty {
                    ty::FnConverging(ty) => Some(miri.memory.allocate(Repr::from_ty(ty))),
                    ty::FnDiverging => None,
                };
                miri.call(mir, &[], return_ptr.clone()).unwrap();

                if let Some(ret) = return_ptr {
                    println!("Returned: {:?}\n", miri.memory.get(ret.alloc_id).unwrap());
                }
            }
        }
    }
}

fn check_expected(actual: &str, attr: &Attribute) -> bool {
    if let Some(meta_items) = attr.meta_item_list() {
        for meta_item in meta_items {
            if meta_item.check_name("expected") {
                let expected = meta_item.value_str().unwrap();

                if actual == &expected[..] {
                    println!("Test passed!\n");
                } else {
                    println!("Actual value:\t{}\nExpected value:\t{}\n", actual, expected);
                }

                return true;
            }
        }
    }

    false
}
