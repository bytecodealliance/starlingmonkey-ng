// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
"use strict";
(() => {
  // src/breakpoints.ts
  var MAX_LINE_SLIDE = 20;
  var Breakpoints = class {
    constructor(dbg, sources, onHit) {
      this.dbg = dbg;
      this.sources = sources;
      this.onHit = onHit;
    }
    dbg;
    sources;
    onHit;
    nextId = 1;
    byPath = /* @__PURE__ */ new Map();
    /** Replace the breakpoints in `clientPath`, binding each to loaded code where there is some. */
    set(clientPath, requested) {
      for (const entry of this.byPath.get(clientPath) ?? []) {
        this.unbind(entry);
      }
      const entries = requested.map((breakpoint) => this.create(breakpoint));
      this.byPath.set(clientPath, entries);
      const urls = this.urlsFor(clientPath);
      for (const entry of entries) {
        this.bind(entry, urls);
      }
      return entries.map((entry) => describe(entry));
    }
    /** Bind unbound breakpoints to the scripts of a newly loaded `url`. Returns those it bound. */
    scriptLoaded(url) {
      const changed = [];
      for (const [clientPath, entries] of this.byPath) {
        if (!this.sources.matches(url, clientPath)) {
          continue;
        }
        for (const entry of entries) {
          if (entry.bound === void 0 && this.bind(entry, [url])) {
            changed.push(describe(entry));
          }
        }
      }
      return changed;
    }
    /** The positions on lines `line` through `endLine` of `clientPath` a breakpoint can bind to. */
    locations(clientPath, line, endLine = line) {
      return this.candidates(this.urlsFor(clientPath), line, endLine).map(({ position }) => ({
        line: position.lineNumber,
        column: position.columnNumber
      }));
    }
    clear() {
      for (const entries of this.byPath.values()) {
        for (const entry of entries) {
          this.unbind(entry);
        }
      }
      this.byPath.clear();
    }
    create(requested) {
      const id = this.nextId++;
      return {
        id,
        line: requested.line,
        column: requested.column,
        handler: {
          hit: (frame) => {
            this.onHit(frame, id);
            return void 0;
          }
        },
        bound: void 0
      };
    }
    urlsFor(clientPath) {
      return this.sources.loadedUrls().filter((url) => this.sources.matches(url, clientPath));
    }
    /**
     * Set the breakpoint at the first position on the first line at or after its line that has
     * code, preferring its requested column on that line.
     */
    bind(entry, urls) {
      const candidates = this.candidates(urls, entry.line, entry.line + MAX_LINE_SLIDE);
      if (candidates.length === 0) {
        return false;
      }
      const line = candidates[0].position.lineNumber;
      const onLine = candidates.filter((candidate) => candidate.position.lineNumber === line);
      const chosen = onLine.find((candidate) => candidate.position.columnNumber === entry.column) ?? onLine[0];
      chosen.script.setBreakpoint(chosen.position.offset, entry.handler);
      entry.bound = {
        script: chosen.script,
        line: chosen.position.lineNumber,
        column: chosen.position.columnNumber
      };
      return true;
    }
    unbind(entry) {
      entry.bound?.script.clearBreakpoint(entry.handler);
      entry.bound = void 0;
    }
    /** Breakpoint positions on lines `minLine` through `maxLine`, in source order. */
    candidates(urls, minLine, maxLine) {
      const found = [];
      for (const url of urls) {
        for (const script of this.dbg.findScripts({ url })) {
          for (const position of script.getPossibleBreakpoints({ minLine, maxLine: maxLine + 1 })) {
            found.push({ script, position });
          }
        }
      }
      return found.sort(
        (a, b) => a.position.lineNumber - b.position.lineNumber || a.position.columnNumber - b.position.columnNumber
      );
    }
  };
  function describe(entry) {
    if (entry.bound === void 0) {
      return { id: entry.id, verified: false, line: entry.line, message: "No loaded code at this line" };
    }
    return { id: entry.id, verified: true, line: entry.bound.line, column: entry.bound.column };
  }

  // src/capabilities.ts
  var CAPABILITIES = {
    supportsConfigurationDoneRequest: true,
    supportsTerminateRequest: true,
    supportTerminateDebuggee: true,
    supportsBreakpointLocationsRequest: true,
    supportsSetVariable: true,
    supportsEvaluateForHovers: true,
    supportsLoadedSourcesRequest: true
  };

  // src/sources.ts
  var Sources = class {
    localRoot;
    remoteRoot;
    /** Client paths that matched a URL by suffix, reported back for that URL. */
    clientPaths = /* @__PURE__ */ new Map();
    sourcesByUrl = /* @__PURE__ */ new Map();
    references = /* @__PURE__ */ new Map();
    sourcesByReference = /* @__PURE__ */ new Map();
    nextReference = 1;
    /** Map URLs under `remoteRoot` to paths under `localRoot`. */
    setRoots(localRoot, remoteRoot) {
      this.localRoot = trimTrailingSlash(localRoot);
      this.remoteRoot = trimTrailingSlash(remoteRoot);
    }
    /**
     * Whether `url` names the file the client calls `clientPath`. With roots set, a URL under the
     * remote root matches only its mapped path. Otherwise one path must end with the other, at a
     * path segment boundary.
     */
    matches(url, clientPath) {
      const path = urlPath(url);
      if (path === void 0) {
        return false;
      }
      const mapped = this.mapRemote(path);
      if (mapped !== void 0) {
        return mapped === clientPath;
      }
      if (path === clientPath || endsWithSegments(path, clientPath) || endsWithSegments(clientPath, path)) {
        this.clientPaths.set(url, clientPath);
        return true;
      }
      return false;
    }
    /** Record a loaded source. Returns `true` if its URL was not loaded before. */
    add(source) {
      if (this.sourcesByUrl.has(source.url)) {
        return false;
      }
      this.sourcesByUrl.set(source.url, source);
      return true;
    }
    loadedUrls() {
      return [...this.sourcesByUrl.keys()];
    }
    loaded() {
      return [...this.sourcesByUrl.values()];
    }
    /**
     * The DAP source for `source`. A URL that names a file gets a `path`, which the client opens
     * itself. Any other URL gets a `sourceReference` for the `source` request.
     */
    dapSource(source) {
      const path = this.clientPath(source.url);
      if (path !== void 0) {
        return { name: basename(path), path };
      }
      return { name: source.url, sourceReference: this.reference(source) };
    }
    /** The text of the source with `reference`. */
    text(reference) {
      return this.sourcesByReference.get(reference)?.text;
    }
    reference(source) {
      let reference = this.references.get(source.url);
      if (reference === void 0) {
        reference = this.nextReference++;
        this.references.set(source.url, reference);
        this.sourcesByReference.set(reference, source);
      }
      return reference;
    }
    clientPath(url) {
      const remembered = this.clientPaths.get(url);
      if (remembered !== void 0) {
        return remembered;
      }
      const path = urlPath(url);
      return path === void 0 ? void 0 : this.mapRemote(path) ?? path;
    }
    mapRemote(path) {
      if (this.localRoot === void 0 || this.remoteRoot === void 0) {
        return void 0;
      }
      if (path === this.remoteRoot || path.startsWith(`${this.remoteRoot}/`)) {
        return this.localRoot + path.slice(this.remoteRoot.length);
      }
      return void 0;
    }
  };
  function urlPath(url) {
    if (url.startsWith("file://")) {
      return decodeURIComponent(url.slice("file://".length));
    }
    if (url.startsWith("/") || /^[A-Za-z]:[\\/]/.test(url)) {
      return url;
    }
    return void 0;
  }
  function endsWithSegments(path, suffix) {
    if (!path.endsWith(suffix)) {
      return false;
    }
    const boundary = path.length - suffix.length;
    return boundary === 0 || suffix.startsWith("/") || path[boundary - 1] === "/";
  }
  function basename(path) {
    return path.slice(path.lastIndexOf("/") + 1);
  }
  function trimTrailingSlash(path) {
    return path?.replace(/\/+$/, "");
  }

  // src/stepping.ts
  var Stepper = class {
    constructor(dbg, onStop) {
      this.dbg = dbg;
      this.onStop = onStop;
    }
    dbg;
    onStop;
    hooked = /* @__PURE__ */ new Set();
    start(kind, frame) {
      this.clear();
      if (kind !== "stepOut") {
        this.stopOnNewLine(frame);
      }
      this.continueInCaller(frame);
      if (kind === "stepIn") {
        this.dbg.onEnterFrame = (entered) => {
          this.stop(entered);
          return void 0;
        };
      }
    }
    /** Remove every hook a step set. */
    clear() {
      for (const frame of this.hooked) {
        if (!frame.terminated) {
          frame.onStep = void 0;
          frame.onPop = void 0;
        }
      }
      this.hooked.clear();
      this.dbg.onEnterFrame = void 0;
    }
    stopOnNewLine(frame) {
      this.hooked.add(frame);
      const startLine = lineOf(frame);
      frame.onStep = () => {
        const position = frame.script?.getOffsetMetadata(frame.offset);
        if (position !== void 0 && position.isBreakpoint && position.lineNumber !== startLine) {
          this.stop(frame);
        }
        return void 0;
      };
    }
    /** When `frame` returns or throws, continue the step in its caller. */
    continueInCaller(frame) {
      this.hooked.add(frame);
      frame.onPop = (completion) => {
        if (completion !== null && "return" in completion && (completion.await || completion.yield)) {
          return void 0;
        }
        const caller = frame.older;
        if (caller !== null) {
          this.stopOnNewLine(caller);
          this.continueInCaller(caller);
        }
        return void 0;
      };
    }
    stop(frame) {
      this.clear();
      this.onStop(frame);
    }
  };
  function lineOf(frame) {
    return frame.script?.getOffsetMetadata(frame.offset).lineNumber;
  }

  // src/values.ts
  var References = class {
    containers = /* @__PURE__ */ new Map();
    objectReferences = /* @__PURE__ */ new Map();
    next = 1;
    clear() {
      this.containers.clear();
      this.objectReferences.clear();
      this.next = 1;
    }
    scope(envs, thisValue) {
      return this.add({ kind: "scope", envs, thisValue });
    }
    object(object) {
      let reference = this.objectReferences.get(object);
      if (reference === void 0) {
        reference = this.add({ kind: "object", object });
        this.objectReferences.set(object, reference);
      }
      return reference;
    }
    get(reference) {
      return this.containers.get(reference);
    }
    add(container) {
      const reference = this.next++;
      this.containers.set(reference, container);
      return reference;
    }
  };
  function describe2(value, refs) {
    if (value instanceof Debugger.Object) {
      return {
        value: preview(value),
        type: value.callable ? "function" : value.class,
        variablesReference: refs.object(value)
      };
    }
    if (value !== null && typeof value === "object") {
      if ("optimizedOut" in value) {
        return plain("<optimized out>", "unavailable");
      }
      if ("uninitialized" in value) {
        return plain("<uninitialized>", "uninitialized");
      }
      if ("missingArguments" in value) {
        return plain("<unavailable>", "unavailable");
      }
    }
    switch (typeof value) {
      case "string":
        return plain(JSON.stringify(value), "string");
      case "bigint":
        return plain(`${value}n`, "bigint");
      case "symbol":
        return plain(value.toString(), "symbol");
      case "undefined":
        return plain("undefined", "undefined");
      default:
        return plain(String(value), value === null ? "null" : typeof value);
    }
  }
  function scopesOf(frame, refs) {
    const local = [];
    let env = frame.environment;
    while (env !== null) {
      local.push(env);
      const parent = env.parent;
      const endsLocal = env.callee != null || env.scopeKind === "module" || parent === null || isGlobalObject(parent);
      env = parent;
      if (endsLocal) {
        break;
      }
    }
    const scopes = [
      {
        name: "Local",
        presentationHint: "locals",
        variablesReference: refs.scope(local, frame["this"]),
        expensive: false
      }
    ];
    for (; env !== null; env = env.parent) {
      if (isGlobalObject(env) && env.object !== void 0) {
        scopes.push({ name: "Global", variablesReference: refs.object(env.object), expensive: true });
        break;
      }
      scopes.push({ name: scopeName(env), variablesReference: refs.scope([env]), expensive: false });
    }
    return scopes;
  }
  function variablesOf(container, refs) {
    if (container.kind === "object") {
      const { object } = container;
      const variables2 = object.getOwnPropertyNames().map((name) => property(object, name, refs));
      if (object.proto !== null) {
        variables2.push({ name: "[[Prototype]]", ...describe2(object.proto, refs) });
      }
      return variables2;
    }
    const variables = [];
    if (container.thisValue !== void 0) {
      variables.push({ name: "this", ...describe2(container.thisValue, refs) });
    }
    const seen = /* @__PURE__ */ new Set();
    for (const env of container.envs) {
      if (env.optimizedOut) {
        continue;
      }
      for (const name of env.names()) {
        if (!seen.has(name)) {
          seen.add(name);
          variables.push({ name, ...describe2(env.getVariable(name), refs) });
        }
      }
    }
    return variables;
  }
  function plain(value, type) {
    return { value, type, variablesReference: 0 };
  }
  function preview(object) {
    if (object.callable) {
      return `ƒ ${object.name ?? object.displayName ?? ""}()`;
    }
    switch (object.class) {
      case "Array":
        return `Array(${String(object.getOwnPropertyDescriptor("length")?.value)})`;
      case "Object":
        return "{…}";
      default: {
        if (!object.class.endsWith("Error")) {
          return object.class;
        }
        const message = object.getOwnPropertyDescriptor("message")?.value;
        return message === void 0 ? object.class : `${object.class}: ${String(message)}`;
      }
    }
  }
  function property(object, name, refs) {
    const descriptor = object.getOwnPropertyDescriptor(name);
    if (descriptor !== void 0 && !("value" in descriptor)) {
      const kinds = [descriptor.get && "get", descriptor.set && "set"].filter(Boolean).join("/");
      return { name, value: `(${kinds})`, type: "accessor", variablesReference: 0 };
    }
    return { name, ...describe2(descriptor?.value, refs) };
  }
  function isGlobalObject(env) {
    return env.type === "object" && env.parent === null;
  }
  function scopeName(env) {
    if (env.callee != null) {
      return `Closure (${env.callee.name ?? "anonymous"})`;
    }
    if (env.scopeKind === "module") {
      return "Module";
    }
    if (env.parent !== null && isGlobalObject(env.parent)) {
      return "Script";
    }
    return "Block";
  }

  // src/adapter.ts
  var RESPONDED = /* @__PURE__ */ Symbol("responded");
  var THREAD_ID = 1;
  function describeError(error) {
    return error instanceof Error ? error.message : String(error);
  }
  var Adapter = class {
    constructor(dbg, contentGlobal2) {
      this.dbg = dbg;
      dbg.addAllGlobalsAsDebuggees();
      this.contentGlobal = dbg.addDebuggee(contentGlobal2);
      dbg.onNewGlobalObject = (global) => {
        dbg.addDebuggee(global);
      };
      dbg.uncaughtExceptionHook = (error) => {
        this.output("important", `Debug adapter error: ${describeError(error)}
`);
        return void 0;
      };
      this.on("initialize", () => CAPABILITIES);
      this.on("launch", (request) => this.attach(request));
      this.on("attach", (request) => this.attach(request));
      this.on("configurationDone", () => {
        if (this.launchArguments.stopOnEntry) {
          this.stopAtNextFrame("entry");
        }
        configured();
        return void 0;
      });
      this.on("threads", () => ({ threads: [{ id: THREAD_ID, name: "JavaScript" }] }));
      this.on(
        "disconnect",
        (request) => this.disconnect(request)
      );
      this.on("terminate", () => {
        this.afterResponse(() => terminate());
        return void 0;
      });
      this.breakpoints = new Breakpoints(
        dbg,
        this.sources,
        (frame, id) => this.stop(frame, "breakpoint", { hitBreakpointIds: [id] })
      );
      for (const script of dbg.findScripts()) {
        this.sources.add(script.source);
      }
      dbg.onNewScript = (script) => this.scriptLoaded(script);
      this.onDetach(() => this.breakpoints.clear());
      this.on("setBreakpoints", (request) => {
        const args = request.arguments;
        const requested = args.breakpoints ?? [];
        if (args.source.path === void 0) {
          return {
            breakpoints: requested.map(() => ({
              verified: false,
              message: "Breakpoints need a source path"
            }))
          };
        }
        return { breakpoints: this.breakpoints.set(args.source.path, requested) };
      });
      this.on("breakpointLocations", (request) => {
        const args = request.arguments;
        const path = args?.source.path;
        return {
          breakpoints: args === void 0 || path === void 0 ? [] : this.breakpoints.locations(path, args.line, args.endLine)
        };
      });
      this.on("continue", () => {
        resume();
        return { allThreadsContinued: true };
      });
      this.onStop(() => this.refs.clear());
      this.onResume(() => this.refs.clear());
      this.stepper = new Stepper(dbg, (frame) => this.stop(frame, "step"));
      this.onStop(() => this.stepper.clear());
      this.onDetach(() => this.stepper.clear());
      dbg.onDebuggerStatement = (frame) => {
        this.stop(frame, "debugger statement");
        return void 0;
      };
      for (const kind of ["next", "stepIn", "stepOut"]) {
        this.on(kind, () => this.step(kind));
      }
      this.on("pause", () => {
        if (this.pausedFrame === void 0) {
          this.stopAtNextFrame("pause");
        }
        return void 0;
      });
      this.on("stackTrace", (request) => {
        const args = request.arguments;
        const frames = this.frames();
        const start = args.startFrame ?? 0;
        const end = args.levels ? start + args.levels : frames.length;
        return {
          stackFrames: frames.slice(start, end).map((frame, i) => this.dapFrame(frame, start + i)),
          totalFrames: frames.length
        };
      });
      this.on("scopes", (request) => {
        const args = request.arguments;
        return { scopes: scopesOf(this.frame(args.frameId), this.refs) };
      });
      this.on("variables", (request) => {
        const args = request.arguments;
        const container = this.refs.get(args.variablesReference);
        if (container === void 0) {
          throw new Error("Unknown variables reference");
        }
        return { variables: variablesOf(container, this.refs) };
      });
      this.on(
        "setVariable",
        (request) => this.setVariable(request.arguments)
      );
      this.on("evaluate", (request) => {
        const args = request.arguments;
        const completion = this.pausedFrame !== void 0 ? this.frame(args.frameId).eval(args.expression) : this.contentGlobal.executeInGlobal(args.expression);
        const { value, type, variablesReference } = describe2(this.completionValue(completion), this.refs);
        return { result: value, type, variablesReference };
      });
      this.on("source", (request) => {
        const args = request.arguments;
        const reference = args.source?.sourceReference ?? args.sourceReference;
        const content = this.sources.text(reference);
        if (content === void 0) {
          throw new Error(`Unknown source reference ${reference}`);
        }
        return { content, mimeType: "text/javascript" };
      });
      this.on("loadedSources", () => ({
        sources: this.sources.loaded().map((source) => this.sources.dapSource(source))
      }));
    }
    dbg;
    handlers = /* @__PURE__ */ new Map();
    afterResponseActions = [];
    detachActions = [];
    detached = false;
    sources = new Sources();
    breakpoints;
    stopActions = [];
    resumeActions = [];
    refs = new References();
    stepper;
    /** The frame the debuggee is paused in, while a pause is active. */
    pausedFrame;
    launchArguments = {};
    /** The global `evaluate` and `setVariable` run in while nothing is paused. */
    contentGlobal;
    on(command, handler) {
      this.handlers.set(command, handler);
    }
    /** Run `action` after the current request's response is sent. */
    afterResponse(action) {
      this.afterResponseActions.push(action);
    }
    /** Run `action` when the client detaches. */
    onDetach(action) {
      this.detachActions.push(action);
    }
    handle(message) {
      if (message.type !== "request") {
        return;
      }
      const request = message;
      const handler = this.handlers.get(request.command);
      if (handler === void 0) {
        this.respondError(request, `Unsupported request '${request.command}'`);
        return;
      }
      let body;
      try {
        body = handler(request);
      } catch (error) {
        this.afterResponseActions.length = 0;
        this.respondError(request, describeError(error));
        return;
      }
      if (body !== RESPONDED) {
        this.respond(request, body);
      }
      for (const action of this.afterResponseActions.splice(0)) {
        action();
      }
    }
    respond(request, body) {
      send(
        JSON.stringify({
          type: "response",
          request_seq: request.seq,
          command: request.command,
          success: true,
          body
        })
      );
    }
    respondError(request, message) {
      send(
        JSON.stringify({
          type: "response",
          request_seq: request.seq,
          command: request.command,
          success: false,
          message
        })
      );
    }
    sendEvent(event, body) {
      send(JSON.stringify({ type: "event", event, body }));
    }
    output(category, output) {
      this.sendEvent("output", { category, output });
    }
    /** Run `action` when the debuggee stops, before the `stopped` event is sent. */
    onStop(action) {
      this.stopActions.push(action);
    }
    /** Run `action` when the debuggee resumes from a stop. */
    onResume(action) {
      this.resumeActions.push(action);
    }
    /** Pause in `frame` until the client resumes. Does nothing while already paused. */
    stop(frame, reason, extra = {}) {
      if (this.pausedFrame !== void 0) {
        return;
      }
      if (!mayPause()) {
        this.output("console", `Skipped a ${reason} stop: this code cannot pause.
`);
        return;
      }
      for (const action of this.stopActions) {
        action();
      }
      this.pausedFrame = frame;
      this.sendEvent("stopped", { reason, threadId: THREAD_ID, allThreadsStopped: true, ...extra });
      try {
        pause();
      } finally {
        this.pausedFrame = void 0;
        for (const action of this.resumeActions) {
          action();
        }
      }
    }
    step(kind) {
      if (this.pausedFrame === void 0) {
        throw new Error("The debuggee is not paused");
      }
      this.stepper.start(kind, this.pausedFrame);
      resume();
      return void 0;
    }
    /** Stop in the next content frame that runs. */
    stopAtNextFrame(reason) {
      this.dbg.onEnterFrame = (frame) => {
        this.dbg.onEnterFrame = void 0;
        this.stop(frame, reason);
        return void 0;
      };
    }
    scriptLoaded(script) {
      if (script.source.introductionType === "debugger eval") {
        return;
      }
      if (this.sources.add(script.source)) {
        this.sendEvent("loadedSource", { reason: "new", source: this.sources.dapSource(script.source) });
      }
      for (const breakpoint of this.breakpoints.scriptLoaded(script.url)) {
        this.sendEvent("breakpoint", { reason: "changed", breakpoint });
      }
    }
    attach(request) {
      this.launchArguments = request.arguments ?? {};
      this.sources.setRoots(this.launchArguments.localRoot, this.launchArguments.remoteRoot);
      this.afterResponse(() => this.sendEvent("initialized"));
      return void 0;
    }
    disconnect(request) {
      if (request.arguments?.terminateDebuggee) {
        this.afterResponse(() => terminate());
        return void 0;
      }
      if (this.detached) {
        return request.seq === 0 ? RESPONDED : void 0;
      }
      this.detached = true;
      for (const action of this.detachActions) {
        action();
      }
      this.dbg.onNewScript = void 0;
      this.dbg.onEnterFrame = void 0;
      this.dbg.onDebuggerStatement = void 0;
      this.dbg.onNewGlobalObject = void 0;
      this.dbg.removeAllDebuggees();
      configured();
      resume();
      this.afterResponse(() => detached());
      return void 0;
    }
    /** The paused stack, innermost frame first. */
    frames() {
      if (this.pausedFrame === void 0) {
        throw new Error("The debuggee is not paused");
      }
      const frames = [];
      for (let frame = this.pausedFrame; frame !== null; frame = frame.older) {
        frames.push(frame);
      }
      return frames;
    }
    /** The paused frame with DAP id `frameId`, or the innermost one without an id. */
    frame(frameId) {
      const frame = this.frames()[(frameId ?? 1) - 1];
      if (frame === void 0) {
        throw new Error(`Unknown frame ${frameId}`);
      }
      return frame;
    }
    dapFrame(frame, index) {
      const id = index + 1;
      const name = frame.callee !== null ? frame.callee.name ?? frame.callee.displayName ?? "(anonymous)" : `(${frame.type})`;
      if (frame.script === null) {
        return { id, name, line: 0, column: 0 };
      }
      const position = frame.script.getOffsetMetadata(frame.offset);
      return {
        id,
        name,
        line: position.lineNumber,
        column: position.columnNumber,
        source: this.sources.dapSource(frame.script.source)
      };
    }
    /** The value of a completion, or an `Error` describing why there is none. */
    completionValue(completion) {
      if (completion === null) {
        throw new Error("Evaluation was terminated");
      }
      if ("throw" in completion) {
        throw new Error(`Uncaught ${describe2(completion.throw, this.refs).value}`);
      }
      return completion.return;
    }
    setVariable(args) {
      const container = this.refs.get(args.variablesReference);
      if (container === void 0) {
        throw new Error("Unknown variables reference");
      }
      const value = this.completionValue(
        this.pausedFrame !== void 0 ? this.frame(void 0).eval(args.value) : this.contentGlobal.executeInGlobal(args.value)
      );
      let current;
      if (container.kind === "object") {
        this.completionValue(container.object.setProperty(args.name, value));
        current = container.object.getOwnPropertyDescriptor(args.name)?.value;
      } else {
        const env = container.envs.find((candidate) => candidate.names().includes(args.name));
        if (env === void 0) {
          throw new Error(`No variable named '${args.name}'`);
        }
        env.setVariable(args.name, value);
        current = env.getVariable(args.name);
      }
      const { value: shown, type, variablesReference } = describe2(current, this.refs);
      return { value: shown, type, variablesReference };
    }
  };

  // src/main.ts
  var adapter = new Adapter(new Debugger(), contentGlobal);
  globalThis.handleMessage = (json) => {
    adapter.handle(JSON.parse(json));
  };
})();
