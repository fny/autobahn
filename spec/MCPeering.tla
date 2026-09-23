---- MODULE MCPeering ----
EXTENDS Peering, TLC
CONSTANTS b1, b2
MCPaths == {<<"p">>, <<"q">>}
MCOrder == <<b1, b2>>
\* Generations only grow with the ancestor; a cap keeps a misbehaving
\* model from running away rather than shaping the checked space.
GenBound == \A h \in Hosts, b \in Betas : own[h][b].gen <= 8 /\ copy[h][b].gen <= 8
====
