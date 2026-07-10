# module-import

Demonstrates userspace module imports: a `program` that imports a `module` and
calls one of its operators.

- `greet.cd` — a `module` unit exporting `oper hello`.
- `app.cd` — a `program` that does `use module greet;` and calls `hello {}`.

Resolution is by convention: `use module greet;` in `app.cd` resolves to the
sibling `greet.cd`. The module's operators are lowered into the program's object
with module-scoped linkage names (`greet$hello`), so two modules can define a
same-named private helper without their symbols colliding.

Run it (either backend):

```sh
coddl run examples/module-import/app.cd
# Hello from the greet module!
```
