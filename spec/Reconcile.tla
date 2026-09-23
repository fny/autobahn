---------------------------- MODULE Reconcile ----------------------------
(***************************************************************************)
(* The reconciler's rules over a hierarchy of paths, as pure operators:   *)
(* what one cycle of one pair does to its two trees and its ancestor.     *)
(* Shared by Autobahn.tla (the star, with a fixed leader) and Peering.tla *)
(* (the star, with failover). See Autobahn.tla for the reading of the     *)
(* rules; nothing here refers to a variable.                              *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, Sequences

CONSTANTS
    Paths,      \* prefix-closed sequences of names from the root
    Values,     \* file contents
    NoFile,     \* the content of a path that does not exist
    Dir,        \* the content of a directory
    Mode        \* "conflict" | "alpha" | "strict"

ASSUME Mode \in {"conflict", "alpha", "strict"}
ASSUME NoFile \notin Values /\ Dir \notin Values /\ NoFile # Dir
ASSUME \A p \in Paths : Len(p) >= 1
ASSUME \A p \in Paths : Len(p) > 1 => SubSeq(p, 1, Len(p) - 1) \in Paths

Content == Values \cup {NoFile, Dir}

\* The hierarchy.
Above(p) == {SubSeq(p, 1, k) : k \in 1..(Len(p) - 1)}      \* strict prefixes
Prefixes(p) == Above(p) \cup {p}
Subtree(q) == {p \in Paths : q \in Prefixes(p)}             \* q and below
Below(q) == Subtree(q) \ {q}
TopLevel(p) == Len(p) = 1

Empty == [p \in Paths |-> NoFile]

\* The file values in a subtree of a tree, as <<path, value>>.
FilesIn(t, q) == {<<r, t[r]>> : r \in {r \in Subtree(q) : t[r] \in Values}}

\* A tree with the subtree at q replaced by `content` at q and nothing below.
Cleared(t, q, content) ==
    [r \in Paths |-> IF r = q THEN content ELSE IF r \in Below(q) THEN NoFile ELSE t[r]]

Formed(t) == \A p \in Paths : t[p] # NoFile => (IF TopLevel(p) THEN TRUE ELSE t[SubSeq(p, 1, Len(p) - 1)] = Dir)

-----------------------------------------------------------------------------
\* Where the two sides first disagree on the way down to p, if anywhere:
\* the unit p is decided in. Two directories agree; so do two absences,
\* and two files of the same value.
UnitOf(x, y, p) ==
    LET disagreeing == {q \in Prefixes(p) : x[q] # y[q]} IN
    IF disagreeing = {} THEN <<>>
    ELSE CHOOSE q \in disagreeing : \A r \in disagreeing : Len(q) <= Len(r)

\* One side's changes within a unit since the ancestor, split into the
\* removals and everything else.
Changed(a, t, q) == {r \in Subtree(q) : t[r] # a[r]}
Removed(a, t, q) == {r \in Changed(a, t, q) : t[r] = NoFile}
Kept(a, t, q) == Changed(a, t, q) \ Removed(a, t, q)

\* Whose version of a unit the pair ends up with: "alpha", "beta", or
\* "conflict" for neither. The reconciler's handle_disagreement, for the
\* two-way modes.
Decide(a, x, y, q) ==
    IF Changed(a, y, q) = {} THEN "alpha"          \* beta untouched: alpha's
    ELSE IF Changed(a, x, q) = {} THEN "beta"      \* alpha untouched: beta's
    ELSE IF Kept(a, x, q) = {} /\ Kept(a, y, q) = {} THEN
        \* both only removed: the union of the removals — whichever side
        \* is gone at the top of the unit is what the other side becomes
        IF x[q] = NoFile THEN "alpha" ELSE "beta"
    ELSE IF Kept(a, y, q) = {} THEN "alpha"        \* beta only removed
    ELSE IF Kept(a, x, q) = {} THEN                \* alpha only removed
        IF Mode = "strict" THEN "alpha" ELSE "beta"
    ELSE IF Mode = "conflict" THEN "conflict" ELSE "alpha"

\* The outcome at one path of a cycle over trees (a, x, y).
Outcome(a, x, y, p) ==
    LET q == UnitOf(x, y, p) IN
    IF q = <<>> THEN                                     \* level here
        [alpha |-> x[p], beta |-> y[p], anc |-> x[p], conflict |-> FALSE]
    ELSE
        LET d == Decide(a, x, y, q) IN
        IF d = "alpha" THEN [alpha |-> x[p], beta |-> x[p], anc |-> x[p], conflict |-> FALSE]
        ELSE IF d = "beta" THEN [alpha |-> y[p], beta |-> y[p], anc |-> y[p], conflict |-> FALSE]
        ELSE [alpha |-> x[p], beta |-> y[p], anc |-> a[p], conflict |-> (p = q)]

\* What a cycle took from a tree and carried nowhere: a value that was
\* this side's own change, and after the cycle is on neither side of the
\* pair. Read off the trees, not the rules — the invariants are checks on
\* the rules, not restatements of them.
Lost(side, before, after, other, a) ==
    {<<side, r, before[r]>> : r \in {r \in Paths :
        /\ before[r] \in Values
        /\ before[r] # a[r]
        /\ after[r] # before[r]
        /\ other[r] # before[r]}}

=============================================================================
