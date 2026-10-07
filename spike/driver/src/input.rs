pub const INPUT: &str = r####"
builtin.module @m {
  ^entry():
  llvm.func @callee: llvm.func <builtin.integer i64 (builtin.integer i64) variadic = false> [] {
    ^entry(a: builtin.integer i64):
    llvm.return a
  };
  llvm.func @f: llvm.func <builtin.integer i64 (builtin.integer i64) variadic = false> [] {
    ^entry(x: builtin.integer i64):
    y = builtin.constant <builtin.integer <1: i64>> : builtin.integer i64;
    one = builtin.constant <builtin.integer <1: i1>> : builtin.integer i1;
    llvm.cond_br if one ^bb0(x, y) else ^bb1(x, y)

    ^bb0(x0: builtin.integer i64, y0: builtin.integer i64):
    llvm.br ^bb2(y0, y0)

    ^bb1(x1: builtin.integer i64, y1: builtin.integer i64):
    llvm.br ^bb2(x1, y1)

    ^bb2(x2: builtin.integer i64, y2: builtin.integer i64):
    z = llvm.add x2, y2 <{nsw=false,nuw=false}> : builtin.integer i64;
    c = llvm.icmp z <SLT> x : builtin.integer i1;
    r = llvm.call @callee (z) : llvm.func <builtin.integer i64 (builtin.integer i64) variadic = false>;
    llvm.return r
  }
}
"####;
