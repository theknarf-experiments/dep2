// A small line tokenizer for the FlowLog/Datalog `.dl` dialect, used to syntax-
// highlight the Rules view. Tokens carry a class consumed by RulesView.module.css.
// Comments (`//` / `#`) and strings are single-line in practice, so tokenizing
// line by line is enough.

export type TokClass =
  | "ws"
  | "comment"
  | "string"
  | "number"
  | "directive" // .decl / .in / .out / .printsize / ...
  | "op" // :- = != >= <= > < + - * / % !
  | "builtin" // string builtins: concat, after_last, ...
  | "agg" // count / sum / min / max
  | "atom" // a relation/predicate name (identifier before "(")
  | "type" // number / float / string in a .decl
  | "bool" // True / False
  | "var" // any other identifier (a variable)
  | "punc";

export interface Tok {
  text: string;
  cls: TokClass;
}

const BUILTINS = new Set([
  "split_nth",
  "starts_with",
  "contains",
  "str_before",
  "replace",
  "before_last",
  "after_last",
  "concat",
]);
const AGGS = new Set(["count", "sum", "min", "max"]);
const TYPES = new Set(["number", "float", "string"]);
const BOOLS = new Set(["True", "False"]);

const isSpace = (c: string) => c === " " || c === "\t";
const isDigit = (c: string) => c >= "0" && c <= "9";
const isIdentStart = (c: string) => /[A-Za-z_]/.test(c);
const isIdent = (c: string) => /[A-Za-z0-9_]/.test(c);

export function tokenizeLine(line: string): Tok[] {
  const toks: Tok[] = [];
  const n = line.length;
  let i = 0;
  while (i < n) {
    const c = line[i];

    if (isSpace(c)) {
      let j = i + 1;
      while (j < n && isSpace(line[j])) j++;
      toks.push({ text: line.slice(i, j), cls: "ws" });
      i = j;
      continue;
    }

    // Line comment to end of line.
    if ((c === "/" && line[i + 1] === "/") || c === "#") {
      toks.push({ text: line.slice(i), cls: "comment" });
      break;
    }

    // String literal (tolerates an unterminated string by running to EOL).
    if (c === '"') {
      let j = i + 1;
      while (j < n) {
        if (line[j] === "\\") {
          j += 2;
          continue;
        }
        if (line[j] === '"') {
          j++;
          break;
        }
        j++;
      }
      toks.push({ text: line.slice(i, j), cls: "string" });
      i = j;
      continue;
    }

    // Directive: a dot followed by letters (vs. the rule-terminating ".").
    if (c === "." && isIdentStart(line[i + 1] ?? "")) {
      let j = i + 1;
      while (j < n && /[a-zA-Z]/.test(line[j])) j++;
      toks.push({ text: line.slice(i, j), cls: "directive" });
      i = j;
      continue;
    }

    if (isDigit(c)) {
      let j = i + 1;
      while (j < n && (isDigit(line[j]) || line[j] === ".")) j++;
      toks.push({ text: line.slice(i, j), cls: "number" });
      i = j;
      continue;
    }

    // Operators (multi-char first).
    if (line.startsWith(":-", i)) {
      toks.push({ text: ":-", cls: "op" });
      i += 2;
      continue;
    }
    if (line.startsWith("!=", i) || line.startsWith(">=", i) || line.startsWith("<=", i)) {
      toks.push({ text: line.slice(i, i + 2), cls: "op" });
      i += 2;
      continue;
    }
    if ("=<>+-*/%!".includes(c)) {
      toks.push({ text: c, cls: "op" });
      i++;
      continue;
    }

    if (isIdentStart(c)) {
      let j = i + 1;
      while (j < n && isIdent(line[j])) j++;
      const word = line.slice(i, j);
      // Is it a call? (next non-space char is "(")
      let k = j;
      while (k < n && isSpace(line[k])) k++;
      const call = line[k] === "(";
      let cls: TokClass = "var";
      if (word === "_") cls = "punc";
      else if (call && BUILTINS.has(word)) cls = "builtin";
      else if (call && AGGS.has(word)) cls = "agg";
      else if (call) cls = "atom";
      else if (BOOLS.has(word)) cls = "bool";
      else if (TYPES.has(word)) cls = "type";
      toks.push({ text: word, cls });
      i = j;
      continue;
    }

    // Punctuation: ( ) , . etc.
    toks.push({ text: c, cls: "punc" });
    i++;
  }
  return toks;
}
