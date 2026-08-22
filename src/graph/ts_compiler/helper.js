// Knobyte TypeScript type-checker helper.
//
// Shipped inside the knobyte binary and run with the user's Node only when the opt-in
// `graph.typescript.compiler: "tsc"` mode (or `--ts-compiler`) is on. It loads the TypeScript
// compiler API from the path it is given (never installs or downloads anything), builds one
// program per tsconfig project (project references, include/exclude, package exports and
// node_modules resolution are the compiler's own), and prints JSON facts for the requested
// files: checker-resolved call targets, checker-rendered signatures (overloads included),
// resolved type aliases, and each file's in-project module dependencies.
//
// Input (stdin, JSON): { root, typescript, requested: [rel], candidates: [rel] }
// Output (stdout, JSON): { version, typescript, files: { rel: FileFacts }, warnings: [string] }
"use strict";

const fs = require("fs");
const path = require("path");

const HELPER_FORMAT = 1;
const IGNORED_DIRECTORIES = new Set([
  ".git", ".hg", ".svn", ".knobyte", ".next", ".nuxt", ".turbo", ".cache", "build", "coverage",
  "dist", "node_modules", "out", "target", "vendor",
]);
const CONFIG_FILE = /^(?:tsconfig(?:\.[^.]+)?|jsconfig)\.json$/u;
const SOURCE_FILE = /\.(?:[cm]?[jt]s|[jt]sx)$/u;

function readInput() {
  return JSON.parse(fs.readFileSync(0, "utf8"));
}

function main() {
  const input = readInput();
  const root = path.resolve(input.root);
  // eslint-disable-next-line import/no-dynamic-require
  const ts = require(input.typescript);
  const warnings = [];
  const norm = (p) => path.resolve(p).split(path.sep).join("/");
  const rootN = norm(root);
  const rel = (abs) => {
    const n = norm(abs);
    return n.startsWith(rootN + "/") ? n.slice(rootN.length + 1) : null;
  };
  const inProject = (abs) => {
    const r = rel(abs);
    return r !== null && !r.split("/").includes("node_modules");
  };
  const candidates = new Set((input.candidates || []).map((r) => norm(path.join(root, r))));
  const requested = new Set((input.requested || []).map((r) => norm(path.join(root, r))));

  // ---- Project discovery (most specific config first, then path order) --------------------
  const configs = [];
  const visit = (dir) => {
    let entries;
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    entries.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
    for (const e of entries) {
      if (e.isDirectory()) {
        if (!IGNORED_DIRECTORIES.has(e.name)) visit(path.join(dir, e.name));
      } else if (e.isFile() && CONFIG_FILE.test(e.name)) {
        configs.push(norm(path.join(dir, e.name)));
      }
    }
  };
  visit(root);

  const projects = [];
  const seen = new Set();
  const queue = [...configs];
  while (queue.length > 0) {
    const configPath = queue.shift();
    if (seen.has(configPath)) continue;
    seen.add(configPath);
    let parsed;
    try {
      parsed = ts.getParsedCommandLineOfConfigFile(configPath, {}, {
        ...ts.sys,
        onUnRecoverableConfigFileDiagnostic: (d) => warnings.push(`${rel(configPath)}: ${ts.flattenDiagnosticMessageText(d.messageText, " ")}`),
      });
    } catch (e) {
      warnings.push(`${rel(configPath)}: ${e && e.message}`);
      continue;
    }
    if (!parsed) continue;
    for (const ref of parsed.projectReferences || []) {
      const refPath = norm(ts.resolveProjectReferencePath(ref));
      if (inProject(refPath) && !seen.has(refPath)) queue.push(refPath);
    }
    if (parsed.fileNames.length > 0) projects.push({ configPath, parsed });
  }
  projects.sort((a, b) => {
    const s = path.dirname(b.configPath).length - path.dirname(a.configPath).length;
    return s || (a.configPath < b.configPath ? -1 : a.configPath > b.configPath ? 1 : 0);
  });

  // Each candidate belongs to the first (most specific) project that includes it.
  const owner = new Map();
  for (const p of projects) {
    for (const f of p.parsed.fileNames) {
      const n = norm(f);
      if (candidates.has(n) && !owner.has(n)) owner.set(n, p);
    }
  }
  const unclaimed = [...candidates].filter((f) => !owner.has(f)).sort();
  if (unclaimed.length > 0) {
    const options = {
      allowJs: true,
      checkJs: false,
      jsx: ts.JsxEmit.Preserve,
      target: ts.ScriptTarget.ESNext,
      module: ts.ModuleKind.ESNext,
      moduleResolution: ts.ModuleResolutionKind.Bundler !== undefined
        ? ts.ModuleResolutionKind.Bundler
        : ts.ModuleResolutionKind.NodeJs,
      esModuleInterop: true,
      allowSyntheticDefaultImports: true,
      resolveJsonModule: true,
      skipLibCheck: true,
      noEmit: true,
    };
    const inferred = { configPath: null, parsed: { fileNames: unclaimed, options, projectReferences: undefined } };
    projects.push(inferred);
    for (const f of unclaimed) owner.set(f, inferred);
  }

  // ---- Extraction --------------------------------------------------------------------------
  const files = {};
  for (const project of projects) {
    const owned = [...requested].filter((f) => owner.get(f) === project);
    if (owned.length === 0) continue;
    let program;
    try {
      const options = { ...project.parsed.options, noEmit: true };
      const host = ts.createCompilerHost(options, true);
      // Resolve imports across project references to their sources, as the language service
      // does, instead of to (possibly unbuilt) declaration outputs.
      host.useSourceOfProjectReferenceRedirect = () => true;
      program = ts.createProgram({
        rootNames: project.parsed.fileNames,
        options,
        projectReferences: project.parsed.projectReferences,
        host,
      });
    } catch (e) {
      warnings.push(`${project.configPath ? rel(project.configPath) : "inferred project"}: ${e && e.message}`);
      continue;
    }
    const checker = program.getTypeChecker();
    const options = program.getCompilerOptions();
    for (const abs of owned) {
      const sf = program.getSourceFile(abs) || program.getSourceFiles().find((s) => norm(s.fileName) === abs);
      if (!sf) continue;
      try {
        files[rel(abs)] = extractFile(ts, checker, sf, options, { rel, inProject, norm });
      } catch (e) {
        warnings.push(`${rel(abs)}: ${e && e.message}`);
      }
    }
  }
  process.stdout.write(JSON.stringify({ version: HELPER_FORMAT, typescript: ts.version, files, warnings }));
}

function extractFile(ts, checker, sf, options, util) {
  const text = sf.text;
  const lineStarts = sf.getLineStarts();
  // 1-based line and UTF-8 byte column (tree-sitter columns are bytes).
  const position = (pos) => {
    const lc = sf.getLineAndCharacterOfPosition(pos);
    const prefix = text.slice(lineStarts[lc.line], pos);
    return { line: lc.line + 1, col: Buffer.byteLength(prefix, "utf8") };
  };
  const facts = { deps: [], global: false, calls: [], decls: [], aliases: [] };

  // ---- Dependencies (module resolution) and global scope --------------------------------
  const deps = new Set();
  const addDep = (spec) => {
    try {
      const r = ts.resolveModuleName(spec, sf.fileName, options, ts.sys).resolvedModule;
      if (r && r.resolvedFileName && util.inProject(r.resolvedFileName)) {
        const target = util.rel(r.resolvedFileName);
        if (target && target !== util.rel(sf.fileName)) deps.add(target);
      }
    } catch {
      // unresolvable: not a dependency
    }
  };
  facts.global = sf.isDeclarationFile || !ts.isExternalModule(sf);
  const depVisit = (node) => {
    if ((ts.isImportDeclaration(node) || ts.isExportDeclaration(node)) && node.moduleSpecifier && ts.isStringLiteral(node.moduleSpecifier)) {
      addDep(node.moduleSpecifier.text);
    } else if (ts.isImportEqualsDeclaration(node) && ts.isExternalModuleReference(node.moduleReference)
      && ts.isStringLiteral(node.moduleReference.expression)) {
      addDep(node.moduleReference.expression.text);
    } else if (ts.isCallExpression(node) && node.arguments.length === 1 && ts.isStringLiteralLike(node.arguments[0])
      && ((ts.isIdentifier(node.expression) && node.expression.text === "require") || node.expression.kind === ts.SyntaxKind.ImportKeyword)) {
      addDep(node.arguments[0].text);
    } else if (ts.isModuleDeclaration(node) && (node.flags & ts.NodeFlags.GlobalAugmentation || ts.isStringLiteral(node.name))) {
      // `declare global` / `declare module "x"` augmentations are visible without an import.
      facts.global = true;
    }
    ts.forEachChild(node, depVisit);
  };
  depVisit(sf);
  for (const ref of sf.referencedFiles || []) {
    const abs = path.resolve(path.dirname(sf.fileName), ref.fileName);
    if (util.inProject(abs)) deps.add(util.rel(abs));
  }
  facts.deps = [...deps].sort();

  // ---- Declarations: checker-rendered signatures --------------------------------------------
  const flags = ts.TypeFormatFlags.NoTruncation;
  const declName = (node) => {
    if (node.name && (ts.isIdentifier(node.name) || ts.isPrivateIdentifier(node.name) || ts.isStringLiteral(node.name))) {
      return node.name.text;
    }
    if (ts.isConstructorDeclaration(node)) return "constructor";
    return null;
  };
  const signaturesOf = (node, name) => {
    let symbol = node.name ? checker.getSymbolAtLocation(node.name) : undefined;
    if (!symbol && ts.isConstructorDeclaration(node)) symbol = node.symbol;
    if (!symbol) return null;
    const type = checker.getTypeOfSymbolAtLocation(symbol, node);
    const sigs = ts.isConstructorDeclaration(node) ? type.getConstructSignatures() : type.getCallSignatures();
    if (!sigs || sigs.length === 0) return null;
    const rendered = sigs.map((s) => `${name}${checker.signatureToString(s, node, flags)}`);
    const last = sigs[sigs.length - 1];
    return {
      signature: rendered.join("; "),
      returnType: checker.typeToString(checker.getReturnTypeOfSignature(last), node, flags),
    };
  };
  const declVisit = (node) => {
    let target = null;
    let name = null;
    if (ts.isFunctionDeclaration(node) || ts.isMethodDeclaration(node) || ts.isConstructorDeclaration(node)) {
      // An overload set is one node: its implementation, whose signature lists the overloads.
      const symbol = node.name ? checker.getSymbolAtLocation(node.name) : node.symbol;
      const decls = (symbol && symbol.declarations ? symbol.declarations : [node])
        .filter((d) => d.getSourceFile() === sf && d.kind === node.kind);
      if (decls.length === 0 || implementation(decls) === node) {
        name = declName(node);
        target = node;
      }
    } else if (ts.isVariableDeclaration(node) && ts.isIdentifier(node.name) && node.initializer
      && (ts.isArrowFunction(node.initializer) || ts.isFunctionExpression(node.initializer))) {
      name = node.name.text;
      target = node;
    } else if (ts.isPropertyDeclaration(node) && node.initializer
      && (ts.isArrowFunction(node.initializer) || ts.isFunctionExpression(node.initializer))) {
      name = declName(node);
      target = node;
    }
    if (target && name) {
      const sig = signaturesOf(target, name);
      if (sig) {
        const at = position(target.name ? target.name.getStart(sf) : target.getStart(sf));
        facts.decls.push({ line: at.line, name, signature: sig.signature, returnType: sig.returnType });
      }
    }
    if (ts.isTypeAliasDeclaration(node) && ts.isTypeReferenceNode(node.type)) {
      const loc = locationOf(resolveSymbol(checker.getSymbolAtLocation(node.type.typeName)), false);
      if (loc) {
        facts.aliases.push({ line: position(node.name.getStart(sf)).line, name: node.name.text, target: loc });
      }
    }
    ts.forEachChild(node, declVisit);
  };

  // ---- Calls: checker-resolved targets ---------------------------------------------------------
  function resolveSymbol(symbol) {
    if (!symbol) return undefined;
    if (symbol.flags & ts.SymbolFlags.Alias) {
      try {
        return checker.getAliasedSymbol(symbol);
      } catch {
        return symbol;
      }
    }
    return symbol;
  }
  function declarationLocation(decl) {
    const file = decl.getSourceFile();
    if (!util.inProject(file.fileName)) return null;
    let name = declName(decl);
    let at = decl.name ? decl.name.getStart(file) : decl.getStart(file);
    if (!name && (ts.isFunctionExpression(decl) || ts.isArrowFunction(decl) || ts.isClassExpression(decl))) {
      const parent = decl.parent;
      if (parent && ts.isVariableDeclaration(parent) && ts.isIdentifier(parent.name)) {
        name = parent.name.text;
        at = parent.name.getStart(file);
      } else if (parent && ts.isPropertyDeclaration(parent) && parent.name && ts.isIdentifier(parent.name)) {
        name = parent.name.text;
        at = parent.name.getStart(file);
      } else if (parent && ts.isExportAssignment(parent)) {
        name = "default";
      }
    }
    if (!name && hasModifier(ts, decl, ts.SyntaxKind.DefaultKeyword)) name = "default";
    if (!name) return null;
    const lc = file.getLineAndCharacterOfPosition(at);
    return { file: util.rel(file.fileName), line: lc.line + 1, name };
  }
  function implementation(decls) {
    const withBody = decls.find((d) => d.body !== undefined && d.body !== null);
    return withBody || decls[0];
  }
  // A symbol's declaration knobyte keeps a node for: the implementation of an overload set; for a
  // construction, the class itself.
  function locationOf(symbol, construct) {
    if (!symbol || !symbol.declarations || symbol.declarations.length === 0) return null;
    let decls = symbol.declarations;
    if (construct) {
      const cls = decls.find((d) => ts.isClassDeclaration(d) || ts.isClassExpression(d));
      if (cls) return declarationLocation(cls);
    }
    decls = decls.filter((d) => !ts.isModuleDeclaration(d) && !ts.isInterfaceDeclaration(d) || decls.length === 1);
    if (decls.length === 0) return null;
    return declarationLocation(implementation(decls));
  }
  function targetsOf(call) {
    const callee = call.expression;
    const nameNode = ts.isPropertyAccessExpression(callee) ? callee.name : callee;
    const construct = ts.isNewExpression(call);
    const out = [];
    const push = (loc) => {
      if (loc && !out.some((o) => o.file === loc.file && o.line === loc.line && o.name === loc.name)) out.push(loc);
    };
    let signature;
    try {
      signature = checker.getResolvedSignature(call);
    } catch {
      signature = undefined;
    }
    const decl = signature && signature.declaration;
    if (decl && !ts.isJSDocSignature(decl)) {
      if (construct && ts.isConstructorDeclaration(decl)) {
        push(declarationLocation(decl.parent));
      } else {
        const sym = decl.symbol || (decl.name && checker.getSymbolAtLocation(decl.name));
        push(sym ? locationOf(sym, construct) : declarationLocation(decl));
      }
    }
    if (out.length === 0) {
      const sym = resolveSymbol(checker.getSymbolAtLocation(nameNode));
      push(locationOf(sym, construct));
    }
    if (out.length === 0 && !construct) {
      // A union-typed callee: every member's signature declaration.
      const type = checker.getTypeAtLocation(callee);
      if (type && type.isUnion && type.isUnion()) {
        for (const member of type.types) {
          for (const s of member.getCallSignatures()) {
            if (s.declaration && !ts.isJSDocSignature(s.declaration)) {
              const sym = s.declaration.symbol;
              push(sym ? locationOf(sym, false) : declarationLocation(s.declaration));
            }
          }
        }
      }
    }
    return out;
  }
  const callVisit = (node) => {
    if (ts.isCallExpression(node) || ts.isNewExpression(node)) {
      const callee = node.expression;
      let name = null;
      if (ts.isPropertyAccessExpression(callee)) name = callee.name.text;
      else if (ts.isIdentifier(callee)) name = callee.text;
      else if (callee.kind === ts.SyntaxKind.SuperKeyword) name = null;
      if (name && !(ts.isIdentifier(callee) && name === "require")) {
        const targets = targetsOf(node);
        if (targets.length > 0) {
          const at = position(node.getStart(sf));
          facts.calls.push({ line: at.line, col: at.col, name, targets });
        }
      }
    }
    ts.forEachChild(node, callVisit);
  };

  declVisit(sf);
  callVisit(sf);
  return facts;
}

function hasModifier(ts, node, kind) {
  const mods = ts.canHaveModifiers && ts.canHaveModifiers(node) ? ts.getModifiers(node) : node.modifiers;
  return !!mods && mods.some((m) => m.kind === kind);
}

try {
  main();
} catch (e) {
  process.stderr.write(`knobyte-ts-helper: ${e && e.stack ? e.stack : e}\n`);
  process.exit(2);
}
