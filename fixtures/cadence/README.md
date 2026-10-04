# Cadence fixtures

Cases for spoken list cadence: "one …, two …, three …" and "first …,
second …, third …" become an ordered list. Same JSON shape and harness as
`fixtures/seam/`, run through the whole pipeline. Three markers minimum, as
the English table says; the cases that must *not* become a list are the
important ones. Run with `cargo test -p macalla-core cadence`.
