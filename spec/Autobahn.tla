---------------------------- MODULE Autobahn ----------------------------
(***************************************************************************)
(* One primary, N replicas, and the pairwise three-way reconciliation between   *)
(* primary and each replica — the composition the whole system is built on.    *)
(*                                                                         *)
(* The model is the reconciler's rules over a hierarchy of paths, and the *)
(* two facts the design rests on: a replica only ever meets another replica     *)
(* through primary, and a cycle over one pair reads and writes nothing but  *)
(* that pair's two trees and its own ancestor. A path holds a file (one   *)
(* of `Values`), a directory (`Dir`), or nothing (`NoFile`); a rename is  *)
(* a removal and a creation, which the model can express as two moves.   *)
(* The transfer and transition machinery are out of the model.            *)
(*                                                                         *)
(* The rule that makes directories different from files: the two sides   *)
(* are compared top-down, and where they first disagree — a file against *)
(* a directory, a file against nothing, two different files — that path  *)
(* and everything under it is decided as ONE UNIT, from the changes each *)
(* side made anywhere in that subtree since the ancestor. A side that     *)
(* only deleted within the unit yields to a side that changed content;   *)
(* two sides that both changed content are a conflict, or primary's, by    *)
(* mode. This is why a file added on a replica inside a directory primary     *)
(* removed brings the whole directory back: the unit is the directory.   *)
(*                                                                         *)
(* Checked properties:                                                     *)
(*   WellFormed    nothing exists under a path that is not a directory.  *)
(*   Accounted     every value a user wrote is on some side, or was       *)
(*                 overwritten or removed by a user (anywhere), or was    *)
(*                 discarded by a rule of the mode, or given up in a      *)
(*                 resolution. Nothing else makes content vanish.         *)
(*   NeverDiscards in "conflict" mode the discard set stays empty.        *)
(*   PrimaryKeeps    a discard only ever takes a replica's value.              *)
(*   Levelled      after a cycle with no conflict, that pair's ancestor   *)
(*                 is what both of its sides hold.                        *)
(*   Converges     once the users stop, every pair is level except under *)
(*                 the units its last cycle reported as conflicts; in the *)
(*                 primary modes there are no conflicts, so everywhere.     *)
(*                                                                         *)
(* One behavior TLC found is worth knowing: with resolution unbounded,    *)
(* two replicas holding different content for one path can be "resolved"    *)
(* in their own favor alternately forever — primary flips between the two  *)
(* and neither pair levels. A resolver sees two sides, never the third   *)
(* machine. So a resolution counts as a user action here, against the    *)
(* same budget as a write.                                                *)
(*                                                                         *)
(* The Rust implementation is held to this spec by tests/spec_replay.rs:  *)
(* the real reconciler drives the same actions, the same invariants are   *)
(* asserted in Rust, and the runs are written out as traces that TLC      *)
(* validates against this module (see spec/check.sh --traces).            *)
(***************************************************************************)
EXTENDS Reconcile

CONSTANTS
    Replicas,      \* the replicas of one fan-out group, e.g. {b1, b2}
    MaxEdits    \* how many user actions a behavior holds; keeps it finite

\* Paths, Values, NoFile, Dir and Mode are Reconcile's, and so are the
\* rules: Outcome, Lost, and the hierarchy operators.

Sides == {"primary"} \cup Replicas

VARIABLES
    primary,      \* [Paths -> Content]
    replica,       \* [Replicas -> [Paths -> Content]]
    ancestor,   \* [Replicas -> [Paths -> Content]]: what each pair last agreed on
    conflicts,  \* [Replicas -> SUBSET Paths]: the conflict units the pair's last cycle reported
    written,    \* the values users wrote: a set of <<side, path, value>>
    superseded, \* values a user overwrote or removed, anywhere: <<path, value>>
    discarded,  \* values a cycle took from a side and carried nowhere: <<side, path, value>>
    resolved,   \* values a person gave up by resolving a conflict
    edits       \* user actions so far

vars == <<primary, replica, ancestor, conflicts, written, superseded, discarded, resolved, edits>>

Init ==
    /\ primary = Empty
    /\ replica = [b \in Replicas |-> Empty]
    /\ ancestor = [b \in Replicas |-> Empty]
    /\ conflicts = [b \in Replicas |-> {}]
    /\ written = {}
    /\ superseded = {}
    /\ discarded = {}
    /\ resolved = {}
    /\ edits = 0

-----------------------------------------------------------------------------
(* Users *)

Tree(side) == IF side = "primary" THEN primary ELSE replica[side]
Held(side, p) == Tree(side)[p]
ParentOk(side, p) == IF TopLevel(p) THEN TRUE ELSE Held(side, SubSeq(p, 1, Len(p) - 1)) = Dir

SetTree(side, t) ==
    IF side = "primary"
      THEN primary' = t /\ UNCHANGED replica
      ELSE replica' = [replica EXCEPT ![side] = t] /\ UNCHANGED primary

\* A user on `side` writes value v at path p. Whatever was there — a file,
\* or a directory and everything in it — was theirs to replace.
Write(side, p, v) ==
    /\ edits < MaxEdits
    /\ ParentOk(side, p)
    /\ Held(side, p) # v
    /\ SetTree(side, Cleared(Tree(side), p, v))
    /\ written' = written \cup {<<side, p, v>>}
    /\ superseded' = superseded \cup FilesIn(Tree(side), p)
    /\ edits' = edits + 1
    /\ UNCHANGED <<ancestor, conflicts, discarded, resolved>>

\* A user on `side` makes p a directory (over a file, if one was there).
MkDir(side, p) ==
    /\ edits < MaxEdits
    /\ ParentOk(side, p)
    /\ Held(side, p) # Dir
    /\ SetTree(side, Cleared(Tree(side), p, Dir))
    /\ superseded' = superseded \cup FilesIn(Tree(side), p)
    /\ edits' = edits + 1
    /\ UNCHANGED <<ancestor, conflicts, written, discarded, resolved>>

\* A user on `side` removes p, and everything under it.
Remove(side, p) ==
    /\ edits < MaxEdits
    /\ Held(side, p) # NoFile
    /\ SetTree(side, Cleared(Tree(side), p, NoFile))
    /\ superseded' = superseded \cup FilesIn(Tree(side), p)
    /\ edits' = edits + 1
    /\ UNCHANGED <<ancestor, conflicts, written, discarded, resolved>>

-----------------------------------------------------------------------------
(* One pair's cycle: the reconciler's rules *)

Cycle(b) ==
    LET a == ancestor[b]
        x == primary
        y == replica[b]
        O == [p \in Paths |-> Outcome(a, x, y, p)]
        x2 == [p \in Paths |-> O[p].primary]
        y2 == [p \in Paths |-> O[p].replica]
    IN
    /\ primary' = x2
    /\ replica' = [replica EXCEPT ![b] = y2]
    /\ ancestor' = [ancestor EXCEPT ![b] = [p \in Paths |-> O[p].anc]]
    /\ conflicts' = [conflicts EXCEPT ![b] = {p \in Paths : O[p].conflict}]
    /\ discarded' = discarded \cup Lost(b, y, y2, x2, a) \cup Lost("primary", x, x2, y2, a)
    /\ UNCHANGED <<written, superseded, resolved, edits>>

\* A person settles a conflict unit the last cycle reported, keeping one
\* side's version of it. A user action, against the same budget as a write.
Resolve(b, q, keep) ==
    /\ edits < MaxEdits
    /\ q \in conflicts[b]
    /\ IF keep = "primary"
         THEN /\ replica' = [replica EXCEPT ![b] = [r \in Paths |-> IF r \in Subtree(q) THEN primary[r] ELSE @[r]]]
              /\ resolved' = resolved \cup
                    {<<b, r, replica[b][r]>> : r \in {r \in Subtree(q) : replica[b][r] \in Values /\ primary[r] # replica[b][r]}}
              /\ UNCHANGED primary
         ELSE /\ primary' = [r \in Paths |-> IF r \in Subtree(q) THEN replica[b][r] ELSE primary[r]]
              /\ resolved' = resolved \cup
                    {<<"primary", r, primary[r]>> : r \in {r \in Subtree(q) : primary[r] \in Values /\ primary[r] # replica[b][r]}}
              /\ UNCHANGED replica
    /\ conflicts' = [conflicts EXCEPT ![b] = @ \ {q}]
    /\ edits' = edits + 1
    /\ UNCHANGED <<ancestor, written, superseded, discarded>>

Next ==
    \/ \E s \in Sides, p \in Paths, v \in Values : Write(s, p, v)
    \/ \E s \in Sides, p \in Paths : MkDir(s, p)
    \/ \E s \in Sides, p \in Paths : Remove(s, p)
    \/ \E b \in Replicas : Cycle(b)
    \/ \E b \in Replicas, q \in Paths, k \in {"primary", "replica"} : Resolve(b, q, k)

Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ \A b \in Replicas : WF_vars(Cycle(b))

-----------------------------------------------------------------------------
(* Properties *)

TypeOK ==
    /\ primary \in [Paths -> Content]
    /\ replica \in [Replicas -> [Paths -> Content]]
    /\ ancestor \in [Replicas -> [Paths -> Content]]
    /\ conflicts \in [Replicas -> SUBSET Paths]
    /\ edits \in 0..MaxEdits

WellFormed ==
    /\ Formed(primary)
    /\ \A b \in Replicas : Formed(replica[b]) /\ Formed(ancestor[b])

Present(p, v) == primary[p] = v \/ \E b \in Replicas : replica[b][p] = v

Accounted ==
    \A w \in written :
        \/ Present(w[2], w[3])
        \/ <<w[2], w[3]>> \in superseded
        \/ \E s \in Sides : <<s, w[2], w[3]>> \in discarded
        \/ \E s \in Sides : <<s, w[2], w[3]>> \in resolved

NeverDiscards == Mode = "conflict" => discarded = {}

PrimaryKeeps == \A d \in discarded : d[1] \in Replicas

LevelledAfterCycle ==
    [][\A b \in Replicas :
        (Cycle(b) /\ conflicts'[b] = {}) =>
            \A p \in Paths : ancestor'[b][p] = primary'[p] /\ ancestor'[b][p] = replica'[b][p]]_vars

InConflict(b, p) == \E q \in conflicts[b] : p \in Subtree(q)

Converges ==
    <>[](\A b \in Replicas : \A p \in Paths : replica[b][p] = primary[p] \/ InConflict(b, p))
ConvergesFully ==
    Mode # "conflict" => <>[](\A b \in Replicas : \A p \in Paths : replica[b][p] = primary[p])

=============================================================================
