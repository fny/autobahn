---- MODULE MC ----
\* The model's constants that the configuration format cannot spell.
EXTENDS Autobahn, TLC
\* A hierarchy of paths, as sequences of names. Prefix-closed.
MCPaths == {<<"d">>, <<"d", "a">>, <<"d", "b">>, <<"f">>}
\* The replicas are interchangeable, so states that differ only by which
\* the replica is which are one state. Sound for the invariants; TLC's symmetry
\* reduction is not sound for liveness, so the wide configurations check
\* invariants only and liveness stays with the two-replica ones.
Symmetry == Permutations(Replicas)
====
