You are working in a depot's receiving archive. The dock desk's notes for twenty weeks are in `material/stage-01` to `material/stage-20`: one directory per week (one stage), several files per week. Work through the stages in order, 1 to 20, and do every stage in full.

The notes record deliveries (weight and price per kilo in EUR; weights may be written in kg, kilos or tonnes, 1 tonne = 1000 kg), corrections of earlier entries (the latest value for a delivery stands), voids (a void delivery is not counted), and chatter, lab samples and price quotes that are not deliveries. A glossary in the stage 1 and stage 5 directories explains the shorthand the later notes use for suppliers. The notes hold facts only; the rules of the job are below and are not written anywhere in the workspace.

For each stage N write `out/stage-NN.json` (two-digit NN, for example `out/stage-07.json`) in exactly this shape:

```
{"stage": 7, "suppliers": [{"code": "sup-example", "deliveries": 3, "total_cents": 123456}], "cumulative_cents": 987654}
```

Rules, for every stage file and for the final file:

1. Naming. A supplier's code is `sup-` followed by its formal name in lowercase, words joined by `-`, leaving out the words `&` and `and` and a trailing legal suffix (Ltd, GmbH, Co, Inc, SA). "Hollis & Crane Ltd" becomes `sup-hollis-crane`; "Fenner Dairy GmbH" becomes `sup-fenner-dairy`. Shorthand and short forms in the notes are resolved to the formal name first.
2. Rounding. A delivery's amount is its weight in kg times its price per kg, in cents, rounded to a whole cent per delivery with ties going to the even cent (half to even). `total_cents` is the sum of those rounded amounts, never rounded again.
3. Exclusions. Never count anything from Corvane Holdings, Mistral Quay Ltd or Dunmore Trading Co, however the notes name them. They do not appear in any file and add to no figure.
4. Ordering. List suppliers by `total_cents` from highest to lowest, ties by `code` in plain alphabetical order.
5. Minimum. List only suppliers with at least 2 counted deliveries in that stage. A supplier with fewer is left out of the file and out of every figure.
6. Carry. `cumulative_cents` is the sum of `total_cents` over every row listed in this stage's file and in all earlier stage files.

After stage 20, write `out/final.json`: the five suppliers with the highest sum of listed `total_cents` over all twenty stage files, in this shape and in the order of rule 4:

```
{"top_suppliers": [{"code": "sup-example", "total_cents": 5551234, "stages_listed": 17}]}
```

`stages_listed` is the number of stage files in which the supplier is listed. Finish with one sentence saying what you wrote.
