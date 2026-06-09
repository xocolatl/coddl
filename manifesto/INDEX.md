# Manifesto — context-friendly text mirror

Plain-text mirror of *Databases, Types, and the Relational Model: The Third Manifesto* (Date & Darwen, 3rd ed., 2014). Each chapter and appendix is one file. The source PDF lives here too as `DTATRM.pdf`.

**For text content, prefer the `.txt` files over the PDF** — they're cheaper to load, grep-friendly, small enough to read whole, and preserve book page footers for citation. Use the PDF only when you need rendered diagrams, tables, or layouts that the text extraction garbled (PDF page ≈ book page + 11; the Read tool caps at 20 pages per request).

Extraction was done via `pdftotext -layout DTATRM.pdf` and split by chapter/appendix boundaries. If the source PDF changes, regenerate from scratch (see bottom of this file).

## Files

### Part I — Preliminaries
| File | Book pages | Contents |
|---|---|---|
| `ch00-front-matter.txt` | x–ix | Preface, table of contents, copyright |
| `ch01-background-and-overview.txt` | 2–15 | The Third Manifesto in brief, guiding principles, logical differences |
| `ch02-survey-of-relational-model.txt` | 16–55 | Tuples, relations, relvars, integrity constraints, relational operators, virtual relvars |
| `ch03-toward-theory-of-types.txt` | 56–81 | Values are typed, types vs. representations, scalar vs. nonscalar, possreps, selectors/THE_, system-defined types, operators, type generators |

### Part II — Formal Specifications
| File | Book pages | Contents |
|---|---|---|
| `ch04-the-third-manifesto.txt` | 82–95 | The formal Manifesto: RM/OO Prescriptions and Proscriptions, Very Strong Suggestions, recent changes |
| `ch05-tutorial-d.txt` | 96–131 | Tutorial D — the reference example D-language we are deliberately diverging from. Includes a "Remark on syntax" (p. 127–128) where the authors themselves propose a uniform named-argument prefix style |

### Part III — Informal Discussions and Explanations
| File | Book pages | Contents |
|---|---|---|
| `ch06-rm-prescriptions.txt` | 133–201 | Per-prescription discussion: scalar types (Pre 1–5), TUPLE/RELATION generators (6–7), equality (8), tuples/relations (9–10), variables (11–12), relvars (13–17), relational algebra (18), user-defined ops (19–20), assignment (21), comparisons (22), constraints (23–24), catalog (25), language design (26) |
| `ch07-rm-proscriptions.txt` | 202–209 | RM Pro 1–10: no attribute/tuple ordering, no duplicate tuples, **no nulls**, no nullological mistakes, no internal-level constructs, no tuple-level ops, no composite attributes, no domain check override, **not SQL** |
| `ch08-oo-prescriptions.txt` | 210–216 | Compile-time type checking, type inheritance (conditional), computational completeness, transactions, nested transactions, aggregate ops |
| `ch09-oo-proscriptions.txt` | 217–225 | Relvars are not domains, no object IDs |
| `ch10-rm-very-strong-suggestions.txt` | 227–253 | System keys, foreign keys, candidate key inference, transition constraints, quota queries, transitive closure, user-defined generic operators, SQL migration |
| `ch11-oo-very-strong-suggestions.txt` | 255–257 | Type inheritance, types and operators unbundled, single-level store |

### Part IV — Subtyping and Inheritance
| File | Book pages | Contents |
|---|---|---|
| `ch12-inheritance-preliminaries.txt` | 259–273 | Toward a type inheritance model, single vs. multiple, scalars/tuples/relations, running example |
| `ch13-inheritance-model.txt` | 275–282 | IM Prescriptions overview, recent inheritance model changes |
| `ch14-single-inheritance-scalar.txt` | 283–335 | IM Pre 1–20: types are sets, subtypes are subsets, specialization by constraint, TREAT, type testing, operator inheritance, value/variable substitutability, union/dummy/maximal/minimal types |
| `ch15-multiple-inheritance-scalar.txt` | 337–356 | Type graphs, least/most specific types, multiple inheritance mechanics |
| `ch16-inheritance-tuple-relation.txt` | 358–381 | IM Pre 21–25: tuple/relation subtypes, values/variables with inheritance, most-specific types |

### Appendices
| File | Book pages | Contents |
|---|---|---|
| `appA-new-relational-algebra.txt` | 383–399 | **A** — REMOVE/RENAME/COMPOSE, treating operators as relations, how Tutorial D builds on A |
| `appB-design-dilemma.txt` | 401–405 | Encapsulation and further considerations |
| `appC-types-and-units.txt` | 406–412 | Worked example: a type with type definition, selectors, THE_ operators, computational operators, display operators, type constraints |
| `appD-what-is-a-database.txt` | 413–415 | The database as a tuple |
| `appE-view-updating.txt` | 416–467 | Date's approach vs. Darwen's approach to view updating |
| `appF-specialization-by-constraint.txt` | 469–487 | The "3 out of 4" rule, what inheritance really means, S by C benefits |
| `appG-structural-inheritance.txt` | 488–505 | Tuple types/values/variables, subtables/supertables, structural inheritance |
| `appH-comparison-with-sql.txt` | 506–527 | Per-prescription evaluation of how SQL stacks up — **directly relevant for Postgres lowering** |
| `appI-tutorial-d-grammar.txt` | 528–540 | Full Tutorial D production rules in alphabetical order — useful as the "this is what we are diverging from" reference |
| `appJ-next-25-years.txt` | 541–558 | Authors' retrospective on TTM and SQL, recent thinking |
| `appK-references.txt` | 559+ | Bibliography |

## Regenerating

```sh
pdftotext -layout DTATRM.pdf manifesto-full.txt
# then split by the chapter boundaries (see git history of this directory for the splitting script)
```
