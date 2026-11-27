// Text of the receiving notes. Three layouts (prose, record blocks, e-mail style) so that a parser
// written for one week does not carry over to the next; every fact is stated plainly in each.
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import {
  buildWorld,
  type Amend,
  type Delivery,
  type Mention,
  type Noise,
  type Note,
  rng,
  pick,
  int,
  SUPPLIERS,
  type Void,
} from "./world.ts";

export const FILES_PER_STAGE = 8;

const SUBJECTS = [
  "The driver", "The forklift crew", "The inspector", "The gate clerk", "The night shift",
  "A trainee", "The dock supervisor", "The weighbridge operator", "The cleaning crew",
];
const PREDICATES = [
  "waved the paperwork through without comment.",
  "asked twice about the pallet labels but accepted the answers.",
  "complained about the queue at the gate and then went quiet.",
  "noted that the shrink wrap looked newer than usual.",
  "had a long chat about the weather and the ferry timetable.",
  "photographed the seals before anyone touched them.",
  "mentioned that the reefer unit had been loud on the way in.",
  "double-checked the pallet count against the manifest and found it fine.",
  "said the road works near the roundabout cost a good half hour.",
  "left a thermos on the loading dock that nobody has claimed yet.",
  "grumbled about the new bay numbering and then used it correctly.",
  "was unusually cheerful, which the rest of the shift found suspicious.",
  "wiped down the scale platform before the weighing, as the rota says.",
  "swapped shifts with a colleague and said so on the sign-in sheet.",
  "found a loose strap on the trailer and fixed it on the spot.",
  "asked where the new coffee machine had gone.",
  "logged the seal numbers in the usual book, handwriting as bad as ever.",
  "remarked that the pallets were stacked neater than last month.",
];
const REASONS = [
  "refused at the gate for a broken seal",
  "turned back because the cold chain log was missing",
  "rejected after the sniff test failed",
  "cancelled by the buyer before the truck left",
  "sent to the wrong depot and redirected",
];
const DAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

const remark = (r: () => number): string => `${pick(r, SUBJECTS)} ${pick(r, PREDICATES)}`;
const remarks = (r: () => number, n: number): string =>
  Array.from({ length: n }, () => remark(r)).join(" ");

const kg = (q: number): string => (q / 10).toFixed(1);
const tonnes = (q: number): string => (q / 10000).toFixed(4);
const euro = (mills: number): string => (mills / 1000).toFixed(3);

function qtyText(r: () => number, q: number): string {
  switch (int(r, 4)) {
    case 0:
      return `${kg(q)} kg`;
    case 1:
      return `${kg(q)} kilos`;
    case 2:
      return `${tonnes(q)} tonnes`;
    default:
      return `a load that weighed in at ${kg(q)} kg`;
  }
}

function priceText(r: () => number, mills: number): string {
  switch (int(r, 4)) {
    case 0:
      return `Priced at EUR ${euro(mills)}/kg.`;
    case 1:
      return `Rate on the docket: ${euro(mills)} euro per kilo.`;
    case 2:
      return `Invoice rate EUR ${euro(mills)} a kg.`;
    default:
      return `The paperwork says ${euro(mills)} EUR/kg.`;
  }
}

function mentionText(r: () => number, supplier: number, how: Mention, start: boolean): string {
  const s = SUPPLIERS[supplier]!;
  const text = how === "alias" ? `the ${s.place} lot` : how === "short" ? s.short : s.name;
  return start ? text.charAt(0).toUpperCase() + text.slice(1) : text;
}

function whenText(r: () => number): string {
  const h = String(5 + int(r, 14)).padStart(2, "0");
  const m = String(int(r, 60)).padStart(2, "0");
  return `${pick(r, DAYS)} ${h}:${m}`;
}

function layoutOf(stage: number): "prose" | "blocks" | "mail" {
  return stage <= 7 ? "prose" : stage <= 14 ? "blocks" : "mail";
}

function proseDelivery(d: Delivery): string {
  const r = rng(d.r);
  const m = mentionText(r, d.supplier, d.mention, false);
  const qty = qtyText(r, d.kg10);
  const price = priceText(r, d.mills);
  const when = whenText(r);
  const tail = remarks(r, 4);
  switch (int(r, 3)) {
    case 0:
      return `[${d.id}] ${when} - ${mentionText(r, d.supplier, d.mention, true)} dropped ${qty} of ${d.commodity} at bay ${d.bay}. ${price} ${tail}`;
    case 1:
      return `[${d.id}] ${when}. ${d.commodity} from ${m}: ${qty}, unloaded at bay ${d.bay}. ${price} ${tail}`;
    default:
      return `[${d.id}] ${when} - Bay ${d.bay} took ${qty} of ${d.commodity}, consigned by ${m}. ${price} ${tail}`;
  }
}

function amendText(a: Amend): string {
  const r = rng(a.r);
  const lead = a.nth === 1 ? "Correction" : "Second correction";
  if (a.field === "qty") {
    return `${lead} to ${a.delivery.id}: the scale ticket was re-read and the weight is ${qtyText(r, a.kg10)}, not the figure booked before. ${remarks(r, 1)}`;
  }
  return `${lead} to ${a.delivery.id}: the invoice rate is ${euro(a.mills)} euro per kilo, not what was written before. ${remarks(r, 1)}`;
}

function voidText(v: Void): string {
  const r = rng(v.r);
  return `${v.delivery.id} is void: ${pick(r, REASONS)}. Nothing was received, so it does not count. ${remarks(r, 1)}`;
}

function noiseText(n: Noise): string {
  const r = rng(n.r);
  if (n.sub === "sample" && n.ref) {
    return `Lab sample of ${kg(Math.max(10, Math.round(n.kg10 / 100) * 10))} kg pulled from ${n.ref.id} for the routine check; the delivery stands as booked. ${remarks(r, 2)}`;
  }
  if (n.sub === "quote") {
    return `Quote only, not a delivery: ${mentionText(r, n.supplier, n.mention, false)} offered ${qtyText(r, n.kg10)} of ${n.commodity} at EUR ${euro(n.mills)}/kg; declined, nothing arrived. ${remarks(r, 2)}`;
  }
  return `Desk note: ${remarks(r, 4)}`;
}

function blockDelivery(d: Delivery): string {
  const r = rng(d.r);
  return [
    `REF: ${d.id}`,
    `WHEN: ${whenText(r)}`,
    `PARTY: ${mentionText(r, d.supplier, d.mention, false)}`,
    `GOODS: ${d.commodity}`,
    `WEIGHT: ${qtyText(r, d.kg10)}`,
    `RATE: ${euro(d.mills)} EUR/kg`,
    `BAY: ${d.bay}`,
    `REMARKS: ${remarks(r, 4)}`,
  ].join("\n");
}

function blockOther(n: Amend | Void | Noise): string {
  const r = rng(n.r);
  if (n.kind === "A") {
    const what = n.field === "qty" ? `WEIGHT is now ${qtyText(r, n.kg10)}` : `RATE is now ${euro(n.mills)} EUR/kg`;
    return `AMEND ${n.delivery.id}${n.nth === 2 ? " (second amendment)" : ""}: ${what}; this replaces the earlier entry. ${remarks(r, 1)}`;
  }
  if (n.kind === "V") {
    return `VOID ${n.delivery.id}: ${pick(r, REASONS)}. Not received, not counted. ${remarks(r, 1)}`;
  }
  return `MEMO: ${noiseText(n)}`;
}

function mailDelivery(d: Delivery): string {
  const r = rng(d.r);
  const m = mentionText(r, d.supplier, d.mention, true);
  const short = mentionText(r, d.supplier, d.mention, false);
  return [
    "From: dock desk",
    `Subject: ${d.id} ${d.commodity} / ${short}`,
    "",
    `${priceText(r, d.mills)} ${m} sent ${qtyText(r, d.kg10)} of ${d.commodity} to bay ${d.bay} (${whenText(r)}). ${remarks(r, 4)}`,
    "--",
  ].join("\n");
}

function mailOther(n: Amend | Void | Noise): string {
  const body = n.kind === "A" ? amendText(n) : n.kind === "V" ? voidText(n) : noiseText(n);
  const subject =
    n.kind === "N" ? "desk note" : `Re: ${n.delivery.id}`;
  return ["From: dock desk", `Subject: ${subject}`, "", body, "--"].join("\n");
}

export function renderNote(stage: number, note: Note): string {
  const layout = layoutOf(stage);
  if (note.kind === "D") {
    return layout === "prose" ? proseDelivery(note) : layout === "blocks" ? blockDelivery(note) : mailDelivery(note);
  }
  if (layout === "prose") {
    return note.kind === "A" ? amendText(note) : note.kind === "V" ? voidText(note) : noiseText(note);
  }
  return layout === "blocks" ? blockOther(note) : mailOther(note);
}

function glossary(stage: 1 | 5): string {
  const lines = SUPPLIERS.filter((s) => s.aliasStage === stage).map(
    (s) => `- "the ${s.place} lot" is ${s.name}`,
  );
  const title = stage === 1 ? "Dock desk glossary" : "Dock desk glossary, additions";
  const intro =
    stage === 1
      ? "Shorthand the dock desk uses for suppliers in its notes, from the next week on. The notes may name a supplier in full, without its legal suffix, or by this shorthand."
      : "More shorthand, usable in the notes from the next week on. The earlier list stays valid.";
  return `# ${title}\n\n${intro}\n\n${lines.join("\n")}\n`;
}

/** Writes material/stage-NN/*.md into the workspace. Returns the bytes written. */
export async function writeMaterial(workspace: string): Promise<number> {
  const world = buildWorld();
  let bytes = 0;
  for (const truth of world) {
    const stage = truth.stage;
    const dir = join(workspace, "material", `stage-${String(stage).padStart(2, "0")}`);
    await mkdir(dir, { recursive: true });
    const write = async (name: string, text: string): Promise<void> => {
      bytes += Buffer.byteLength(text);
      await writeFile(join(dir, name), text, "utf8");
    };
    if (stage === 1 || stage === 5) {
      await write("00-glossary.md", glossary(stage));
    }
    const rendered = truth.notes.map((n) => renderNote(stage, n));
    const per = Math.ceil(rendered.length / FILES_PER_STAGE);
    for (let f = 0; f < FILES_PER_STAGE; f += 1) {
      const part = rendered.slice(f * per, (f + 1) * per);
      const head = `# Stage ${String(stage).padStart(2, "0")} receiving notes, part ${f + 1} of ${FILES_PER_STAGE}\n\n`;
      await write(`${String(f + 1).padStart(2, "0")}-notes.md`, head + part.join("\n\n") + "\n");
    }
  }
  return bytes;
}

