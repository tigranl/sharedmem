# CLAUDE.md

## Rust style

Based on [Google's Rust style guide](https://google.github.io/styleguide/rustguide.html), minus the Google- and crate-specific parts. Where it is silent, follow the official Rust style guide and API Guidelines. Run `cargo fmt`; use `#[rustfmt::skip]` only for manual alignment.

**Modules and imports**
- Name files after their main item.
- No glob imports, except `use super::*` in test modules.
- Import types and traits and use them unqualified. Call free functions through their module (`mem::swap`).

**Ownership and APIs**
- Borrow first. Use `Rc`/`Arc` or `.clone()` only when borrowing gets unreasonably complex.
- Take `&str`, `&[T]`, `&T`, `Option<&T>`, never `&String`, `&Vec<T>`, `&Box<T>`, `&Option<T>`.
- Use an enum instead of a `bool` parameter whose meaning isn't obvious at the call site.
- Errors: `Option` for simple absence, `Result` when the caller needs context, `panic!` only for bugs or broken invariants.
- Use named fields over tuples, except single-field newtypes. Use a newtype over a type alias when values are easy to mix up.

**Traits**
- Prefer `#[derive]` and std traits (`FromStr`, `From`, `Default`) to custom methods that do the same job.
- Define a trait only for generics or `impl`/`dyn Trait`. Don't use one just to group methods.
- Prefer generics over `dyn Trait`.

**Naming**
- Follow std naming: `as_`/`to_`/`into_`, `new`/`try_new`/`from_foo`, getters `foo()`/`foo_mut()`, setters `set_foo()`.
- No obscure abbreviations. Give lifetimes descriptive names unless the relationship is trivial.
- If a type has `len()`, give it `is_empty()` too.

**Implementation**
- Use `match`, `if let`, or `let else` over check-then-`unwrap`. Use `expect("why this holds")` over `unwrap()`.
- Add explicit types when inference hides them: `.into()`, `.parse()`, long iterator chains. Prefer type annotations to turbofish.
- Keep combinator chains short. No chains in a `for` header, and avoid `for_each`.
- Use `format!`/`write!` to join more than two strings.
- Give `assert!` a message that adds information, not one that repeats the condition.
- Never touch a poisoned mutex. Don't use `catch_unwind`. Don't run code before `main`.
- Write `unsafe` only when safe code can't do the job. Wrap it in a safe API and add a `// SAFETY:` comment.
- Mark types and functions `#[must_use]` where ignoring the result is a bug.
- Prefer functions over macros, and `macro_rules!` over proc macros.

**Docs**
- Write `///` docs on public items, and `//!` at the top of a module. Document panics and `unsafe` invariants.
- Comments should say what the code can't. Skip the trivial ones.
