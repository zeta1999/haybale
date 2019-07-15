use llvm_ir::*;
use log::debug;
use std::collections::HashMap;
use std::fmt;
use z3::ast::{Ast, BV, Bool};

use crate::memory::Memory;
use crate::solver::Solver;
use crate::alloc::Alloc;
use crate::varmap::VarMap;
use crate::size::size;

pub struct State<'ctx, 'func> {
    pub ctx: &'ctx z3::Context,
    varmap: VarMap<'ctx>,
    mem: Memory<'ctx>,
    alloc: Alloc,
    solver: Solver<'ctx>,
    backtrack_points: Vec<BacktrackPoint<'ctx, 'func>>,
}

struct BacktrackPoint<'ctx, 'func> {
    /// `Function` in which to resume execution
    in_func: &'func Function,
    /// `Name` of the `BasicBlock` to resume execution at
    next_bb: Name,
    /// `Name` of the `BasicBlock` executed just prior to the `BacktrackPoint`
    prev_bb: Name,
    /// Constraint to add before restarting execution at `next_bb`.
    /// (Intended use of this is to constrain the branch in that direction.)
    // We use owned `Bool`s because:
    //   a) it seems necessary to not use refs, and
    //   b) it seems reasonable for callers to give us ownership of these `Bool`s.
    //       If/when that becomes not reasonable, we should probably use boxed
    //       `Bool`s here rather than making callers copy.
    constraint: Bool<'ctx>,
}

impl<'ctx, 'func> BacktrackPoint<'ctx, 'func> {
    fn new(in_func: &'func Function, next_bb: Name, prev_bb: Name, constraint: Bool<'ctx>) -> Self {
        BacktrackPoint{
            in_func,
            next_bb,
            prev_bb,
            constraint,
        }
    }
}

impl<'ctx, 'func> fmt::Display for BacktrackPoint<'ctx, 'func> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "<BacktrackPoint to execute bb {:?} with constraint {}>", self.next_bb, self.constraint)
    }
}

impl<'ctx, 'func> State<'ctx, 'func> {
    pub fn new(ctx: &'ctx z3::Context) -> Self {
        Self {
            ctx,
            varmap: VarMap::new(),
            mem: Memory::new(ctx),
            alloc: Alloc::new(),
            solver: Solver::new(ctx),
            backtrack_points: Vec::new(),
        }
    }

    /// Add `cond` as a constraint, i.e., assert that `cond` must be true
    pub fn assert(&mut self, cond: &Bool<'ctx>) {
        self.solver.assert(cond)
    }

    /// Returns `true` if current constraints are satisfiable, `false` if not.
    /// This function caches its result and will only call to Z3 if constraints have changed
    /// since the last call to `check()`.
    pub fn check(&mut self) -> bool {
        self.solver.check()
    }

    /// Returns `true` if the current constraints plus the additional constraints `conds`
    /// are together satisfiable, or `false` if not.
    /// Does not permanently add the constraints in `conds` to the solver.
    pub fn check_with_extra_constraints(&mut self, conds: &[&Bool<'ctx>]) -> bool {
        self.solver.check_with_extra_constraints(conds)
    }

    /// Get one possible concrete value for the `BV`.
    /// Returns `None` if no possible solution.
    pub fn get_a_solution_for_bv(&mut self, bv: &BV<'ctx>) -> Option<u64> {
        self.solver.get_a_solution_for_bv(bv)
    }

    /// Get one possible concrete value for the `Bool`.
    /// Returns `None` if no possible solution.
    pub fn get_a_solution_for_bool(&mut self, b: &Bool<'ctx>) -> Option<bool> {
        self.solver.get_a_solution_for_bool(b)
    }

    /// Get one possible concrete value for the given IR `Name`, which represents a bitvector.
    /// Returns `None` if no possible solution.
    pub fn get_a_solution_for_bv_by_irname(&mut self, name: &Name) -> Option<u64> {
        let bv = self.varmap.lookup_bv_var(name).clone();  // clone() so that the borrow of self is released
        self.get_a_solution_for_bv(&bv)
    }

    /// Get one possible concrete value for the given IR `Name`, which represents a bool.
    /// Returns `None` if no possible solution.
    pub fn get_a_solution_for_bool_by_irname(&mut self, name: &Name) -> Option<bool> {
        let b = self.varmap.lookup_bool_var(name).clone();  // clone() so that the borrow of self is released
        self.get_a_solution_for_bool(&b)
    }

    /// Associate the given name with the given `BV`
    pub fn add_bv_var(&mut self, name: Name, bv: BV<'ctx>) {
        self.varmap.add_bv_var(name, bv)
    }

    /// Associate the given name with the given `Bool`
    pub fn add_bool_var(&mut self, name: Name, b: Bool<'ctx>) {
        self.varmap.add_bool_var(name, b)
    }

    /// Record the result of `thing` to be `resultval`
    pub fn record_bv_result(&mut self, thing: &impl instruction::HasResult, resultval: BV<'ctx>) {
        let bits = size(&thing.get_type());
        let result = BV::new_const(self.ctx, name_to_sym(thing.get_result().clone()), bits as u32);
        self.assert(&result._eq(&resultval));
        self.varmap.add_bv_var(thing.get_result().clone(), result);
    }

    /// Record the result of `thing` to be `resultval`
    pub fn record_bool_result(&mut self, thing: &impl instruction::HasResult, resultval: Bool<'ctx>) {
        assert_eq!(thing.get_type(), Type::bool());
        let result = Bool::new_const(self.ctx, name_to_sym(thing.get_result().clone()));
        self.assert(&result._eq(&resultval));
        self.varmap.add_bool_var(thing.get_result().clone(), result);
    }

    /// Convert an `Operand` to the appropriate `BV`
    /// (all `Operand`s should be either a constant or a variable we previously added to the state).
    pub fn operand_to_bv(&self, op: &Operand) -> BV<'ctx> {
        match op {
            Operand::ConstantOperand(Constant::Int { bits, value }) => BV::from_u64(self.ctx, *value, *bits),
            Operand::LocalOperand { name, .. } => self.varmap.lookup_bv_var(name).clone(),
            Operand::MetadataOperand(_) => panic!("Can't convert {:?} to BV", op),
            _ => unimplemented!("operand_to_bv() for {:?}", op)
        }
    }

    /// Convert an `Operand` to the appropriate `Bool`
    /// (all `Operand`s should be either a constant or a variable we previously added to the state).
    /// This will panic if the `Operand` doesn't have type `Type::bool()`
    pub fn operand_to_bool(&self, op: &Operand) -> Bool<'ctx> {
        match op {
            Operand::ConstantOperand(Constant::Int { bits, value }) => {
                assert_eq!(*bits, 1);
                Bool::from_bool(self.ctx, *value != 0)
            },
            Operand::LocalOperand { name, .. } => self.varmap.lookup_bool_var(name).clone(),
            op => panic!("Can't convert {:?} to Bool", op),
        }
    }

    /// Read a value `bits` bits long from memory at `addr`.
    /// Caller is responsible for ensuring that the read does not cross cell boundaries
    /// (see notes in memory.rs)
    pub fn read(&self, addr: &BV<'ctx>, bits: u32) -> BV<'ctx> {
        self.mem.read(addr, bits)
    }

    /// Write a value into memory at `addr`.
    /// Caller is responsible for ensuring that the write does not cross cell boundaries
    /// (see notes in memory.rs)
    pub fn write(&mut self, addr: &BV<'ctx>, val: BV<'ctx>) {
        self.mem.write(addr, val)
    }

    /// Allocate a value of size `bits`; return a pointer to the newly allocated object
    pub fn allocate(&mut self, bits: impl Into<u64>) -> BV<'ctx> {
        let raw_ptr = self.alloc.alloc(bits);
        BV::from_u64(self.ctx, raw_ptr, 64)
    }

    // The constraint will be added only if we end up backtracking to this point, and only then
    pub fn save_backtracking_point(&mut self, in_func: &'func Function, next_bb: Name, prev_bb: Name, constraint: Bool<'ctx>) {
        debug!("Saving a backtracking point, which would enter bb {:?} with constraint {}", next_bb, constraint);
        self.solver.push();
        self.backtrack_points.push(BacktrackPoint::new(in_func, next_bb, prev_bb, constraint));
    }

    // returns the Function and BasicBlock where execution should continue, and the BasicBlock executed before that
    // or `None` if there are no saved backtracking points left
    pub fn revert_to_backtracking_point(&mut self) -> Option<(&'func Function, Name, Name)> {
        if let Some(bp) = self.backtrack_points.pop() {
            debug!("Reverting to backtracking point {}", bp);
            self.solver.pop(1);
            debug!("Constraints are now:\n{}", self.solver);
            self.assert(&bp.constraint);
            Some((bp.in_func, bp.next_bb, bp.prev_bb))
            // thanks to SSA, we don't need to roll back the VarMap; we'll just overwrite existing entries as needed.
            // Code on the backtracking path will never reference variables which we assigned on the original path.
            // This will become not true when we get to loops, but we don't support loops yet anyway
        } else {
            None
        }
    }
}

/// Convert an `llvm_ir::Name` to a `z3::Symbol`
// TODO this probably doesn't belong here
pub(crate) fn name_to_sym(name: Name) -> z3::Symbol {
    match name {
        Name::Name(s) => z3::Symbol::String(s),
        Name::Number(n) => z3::Symbol::Int(n as u32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // we don't include tests here for Solver, Memory, Alloc, or VarMap; those are tested in their own modules.
    // Instead, here we just test the nontrivial functionality that State has itself.

    #[test]
    fn lookup_vars_via_operand() {
        let ctx = z3::Context::new(&z3::Config::new());
        let mut state = State::new(&ctx);

        // create llvm-ir values
        let val = Name::Name("val".to_owned());
        let boolval = Name::Number(2);

        // create Z3 values
        let x = BV::new_const(&ctx, "x", 64);
        let boolvar = Bool::new_const(&ctx, "bool");

        // associate llvm-ir values with Z3 values
        state.add_bv_var(val.clone(), x.clone());  // these clone()s wouldn't normally be necessary but we want to compare against the original values later
        state.add_bool_var(boolval.clone(), boolvar.clone());

        // check that we can look up the correct Z3 values via LocalOperands
        let valop = Operand::LocalOperand { name: val, ty: Type::i32() };
        let boolop = Operand::LocalOperand { name: boolval, ty: Type::bool() };
        assert_eq!(state.operand_to_bv(&valop), x);
        assert_eq!(state.operand_to_bool(&boolop), boolvar);
    }

    #[test]
    fn const_bv() {
        let ctx = z3::Context::new(&z3::Config::new());
        let mut state = State::new(&ctx);

        // create an llvm-ir value which is constant 3
        let constint = Constant::Int { bits: 64, value: 3 };

        // this should create a corresponding Z3 value which is also constant 3
        let bv = state.operand_to_bv(&Operand::ConstantOperand(constint));

        // check that the Z3 value was evaluated to 3
        assert_eq!(state.get_a_solution_for_bv(&bv), Some(3));
    }

    #[test]
    fn const_bool() {
        let ctx = z3::Context::new(&z3::Config::new());
        let mut state = State::new(&ctx);

        // create llvm-ir constants true and false
        let consttrue = Constant::Int { bits: 1, value: 1 };
        let constfalse = Constant::Int { bits: 1, value: 0 };

        // this should create Z3 values true and false
        let bvtrue = state.operand_to_bool(&Operand::ConstantOperand(consttrue));
        let bvfalse = state.operand_to_bool(&Operand::ConstantOperand(constfalse));

        // check that the Z3 values are evaluated to true and false respectively
        assert_eq!(state.get_a_solution_for_bool(&bvtrue), Some(true));
        assert_eq!(state.get_a_solution_for_bool(&bvfalse), Some(false));

        // assert the first one, which should be true, so we should still be sat
        state.assert(&bvtrue);
        assert!(state.check());

        // assert the second one, which should be false, so we should be unsat
        state.assert(&bvfalse);
        assert!(!state.check());
    }

    #[test]
    fn backtracking() {
        let ctx = z3::Context::new(&z3::Config::new());
        let mut state = State::new(&ctx);

        // assert x > 11
        let x = BV::new_const(&ctx, "x", 64);
        state.assert(&x.bvsgt(&BV::from_i64(&ctx, 11, 64)));

        // create a Function and some BasicBlocks
        let func = Function::new("test_func");
        let bb1 = BasicBlock::new(Name::Name("bb1".to_owned()));
        let bb2 = BasicBlock::new(Name::Name("bb2".to_owned()));

        // create a backtrack point with constraint y > 5
        let y = BV::new_const(&ctx, "y", 64);
        let constraint = y.bvsgt(&BV::from_i64(&ctx, 5, 64));
        state.save_backtracking_point(&func, bb2.name.clone(), bb1.name.clone(), constraint);

        // check that the constraint y > 5 wasn't added: adding y < 4 should keep us sat
        assert!(state.check_with_extra_constraints(&[&y.bvslt(&BV::from_i64(&ctx, 4, 64))]));

        // assert x < 8 to make us unsat
        state.assert(&x.bvslt(&BV::from_i64(&ctx, 8, 64)));
        assert!(!state.check());

        // roll back to backtrack point; check that we got the right func and bbs
        let (new_func, bb_a, bb_b) = state.revert_to_backtracking_point().unwrap();
        assert_eq!(new_func, &func);
        assert_eq!(bb_a, bb2.name);
        assert_eq!(bb_b, bb1.name);

        // check that the constraint x < 8 was removed: we're sat again
        assert!(state.check());

        // check that the constraint y > 5 was added: y evaluates to something > 5
        assert!(state.get_a_solution_for_bv(&y).unwrap() > 5);

        // check that the first constraint remained in place: x > 11
        assert!(state.get_a_solution_for_bv(&x).unwrap() > 11);

        // check that trying to backtrack again returns None
        assert_eq!(state.revert_to_backtracking_point(), None);
    }
}
