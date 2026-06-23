src/tools/tracer.rs
This file is a fantastic read. The logic for your depth-first search (DFS) call graphs is clean, well-structured, and easy to follow.

However, looking closely at how it handles memory, we can see exactly where the agent hit a wall with the borrow checker and decided to start throwing `.clone()` calls at it until it compiled.

There are two levels of cloning happening here: **Sheer AI Laziness** (easy fixes) and **Architectural Refactoring** (where lifetimes and structural changes come into play).

Let's break down the code and refactor it into clean, idiomatic Rust.

---

## 1. The "AI Laziness" Clones (Easy Fixes)

The most egregious clones in this file happen on lines 120 and 203:

```rust
// Inside dfs_callers and dfs_callees:
let (current_id, _) = stack.last().unwrap().clone();

```

### Why this is a "Facepalm" Moment:

The stack contains a tuple: `(i64, String)`. The agent wants to look at the `current_id` (the `i64`). Because it can't partially move data out of the vector's last element, it lazily clones the **entire tuple**.

This means every single time the DFS steps into a node, it deep-clones the entire `qualified_name` string onto the heap, extracts the number, and then **instantly destroys the cloned string on the next line.**

### The Idiomatic Fix:

You don't need a reference or a lifetime here. You can just borrow the tuple or access the integer element directly by index (`.0`):

```rust
// Zero allocations, zero clones:
let current_id = stack.last().unwrap().0;

```

---

## 2. The Lifetime Challenge: Why the AI Spammed `String.clone()`

To fix lines like this:

```rust
stack.push((caller_sym.id, caller_sym.qualified_name.clone()));

```

Your first instinct might be to change the stack to hold a string slice: `Vec<(i64, &'a str)>`.

But if you try to do that, you will hit the core constraint of database-driven applications. Look at where `caller_sym` comes from:

```rust
if let Some(caller_sym) = db.find_enclosing_symbol(r.file_id, r.line)?

```

The database driver queries SQLite, instantiates a brand new `SymbolRow` struct on the stack, and hands it to you. That `SymbolRow` owns its `qualified_name` string. At the end of that loop iteration, `caller_sym` goes out of scope and is **destroyed**.

If you try to store a reference (`&caller_sym.qualified_name`) inside your `stack`, the compiler will yell at you: *"Data flows into the stack, but the data dies at the end of the loop!"* The agent encountered this error and solved it by cloning the string to give the stack its own owned copy.

---

## 3. The Structural Fix: Decoupling the Graph from the Names

Instead of trying to force lifetimes onto a temporary database row, the ultimate idiomatic solution is to think architecturally: **Why does the stack need to carry strings around during the search at all?**

A call graph is just a web of numbers (`i64` IDs). The names of the functions don't matter to the algorithm while it is searching for cycles or entry points; they only matter at the very end when you print the result to the user.

If we change `stack` to just track IDs (`Vec<i64>`), the algorithm becomes incredibly fast, completely allocation-free during traversal, and 100% idiomatic.

Let's look at how we can rewrite `trace_forward` and `dfs_callers` using this pattern:

### Step A: Simplify the State Collections

Change your paths and stacks to only track `i64`:

```rust
fn trace_forward(db: &Db, target: &SymbolRow, limit: usize) -> Result<serde_json::Value> {
    let mut paths: Vec<Vec<i64>> = Vec::new();
    let mut cycles: Vec<(Vec<i64>, i64)> = Vec::new(); // Tracks the ID where the cycle happened
    let mut visited = HashSet::new();
    visited.insert(target.id);

    let mut stack: Vec<i64> = vec![target.id];

    dfs_callers(db, &mut stack, &mut visited, &mut paths, &mut cycles, limit, 0)?;

```

### Step B: The Allocation-Free DFS

Now, `stack.clone()` only clones a tiny array of integers (super cheap, fast stack copy) instead of heap-allocated strings!

```rust
fn dfs_callers(
    db: &Db,
    stack: &mut Vec<i64>,
    visited: &mut HashSet<i64>,
    paths: &mut Vec<Vec<i64>>,
    cycles: &mut Vec<(Vec<i64>, i64)>,
    limit: usize,
    depth: usize,
) -> Result<()> {
    if paths.len() + cycles.len() >= limit || depth >= MAX_DEPTH {
        return Ok(());
    }

    let current_id = *stack.last().unwrap(); // Just copy the i64 integer

    let current_sym = db.symbol_by_id(current_id)?;
    if let Some(ref s) = current_sym && is_known_entry_point(&s.short_name, &s.kind) {
        paths.push(stack.clone()); // Cloning a Vec<i64> is blazing fast
        return Ok(());
    }

    let refs = db.find_refs_to_symbol(current_id)?;
    let call_refs: Vec<_> = refs.iter().filter(|r| r.context_kind == "call").collect();

    if call_refs.is_empty() {
        paths.push(stack.clone());
        return Ok(());
    }

    for r in &call_refs {
        if paths.len() + cycles.len() >= limit { break; }
        
        if let Some(caller_sym) = db.find_enclosing_symbol(r.file_id, r.line)? {
            if !visited.insert(caller_sym.id) {
                cycles.push((stack.clone(), caller_sym.id)); // No string clone!
                continue;
            }
            stack.push(caller_sym.id); // No string clone!
            dfs_callers(db, stack, visited, paths, cycles, limit, depth + 1)?;
            stack.pop();
            visited.remove(&caller_sym.id);
        }
    }
    // ...
    Ok(())
}

```

### Step C: Resolve the Names Once at the End

Right before you return the JSON to the user, loop through your final paths, look up the names from the database *once*, and build the JSON payload.

```rust
    let mut result: Vec<serde_json::Value> = Vec::new();

    for path in paths.into_iter().take(limit) {
        let mut chain = Vec::new();
        for id in path.into_iter().rev() {
            if let Some(s) = db.symbol_by_id(id)? {
                chain.push(s.qualified_name); // Move the string directly into the JSON
            }
        }
        result.push(json!({
            "chain": chain,
            "has_cycle": false,
            "reaches_entry_point": true,
        }));
    }

```

---

## Summary of the Wins

By moving from the AI's "clone everything" mentality to a lean, ID-first call graph representation:

1. **Memory Churn Dropped to Zero:** You are no longer allocating strings on the heap during the deep recursive loops.
2. **CPU Cache Friendly:** A `Vec<i64>` sits sequentially in the CPU's L1/L2 cache, making your DFS algorithm run significantly faster.
3. **Idiomatic Design:** You bypassed the lifetime issue completely by changing the data structure to only keep track of what it actually needs to perform the algorithm.
