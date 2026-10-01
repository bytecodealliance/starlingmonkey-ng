// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

import type { DebugProtocol } from "@vscode/debugprotocol";

/**
 * Maps between script URLs, which are paths inside the runtime, and the paths the client uses,
 * and tracks the loaded sources.
 */
export class Sources {
  private localRoot: string | undefined;
  private remoteRoot: string | undefined;
  /** Client paths that matched a URL by suffix, reported back for that URL. */
  private readonly clientPaths = new Map<string, string>();
  private readonly sourcesByUrl = new Map<string, Debugger.Source>();
  private readonly references = new Map<string, number>();
  private readonly sourcesByReference = new Map<number, Debugger.Source>();
  private nextReference = 1;

  /** Map URLs under `remoteRoot` to paths under `localRoot`. */
  setRoots(localRoot?: string, remoteRoot?: string): void {
    this.localRoot = trimTrailingSlash(localRoot);
    this.remoteRoot = trimTrailingSlash(remoteRoot);
  }

  /**
   * Whether `url` names the file the client calls `clientPath`. With roots set, a URL under the
   * remote root matches only its mapped path. Otherwise one path must end with the other, at a
   * path segment boundary.
   */
  matches(url: string, clientPath: string): boolean {
    const path = urlPath(url);
    if (path === undefined) {
      return false;
    }
    const mapped = this.mapRemote(path);
    if (mapped !== undefined) {
      return mapped === clientPath;
    }
    if (path === clientPath || endsWithSegments(path, clientPath) || endsWithSegments(clientPath, path)) {
      this.clientPaths.set(url, clientPath);
      return true;
    }
    return false;
  }

  /** Record a loaded source. Returns `true` if its URL was not loaded before. */
  add(source: Debugger.Source): boolean {
    if (this.sourcesByUrl.has(source.url)) {
      return false;
    }
    this.sourcesByUrl.set(source.url, source);
    return true;
  }

  loadedUrls(): string[] {
    return [...this.sourcesByUrl.keys()];
  }

  loaded(): Debugger.Source[] {
    return [...this.sourcesByUrl.values()];
  }

  /**
   * The DAP source for `source`. A URL that names a file gets a `path`, which the client opens
   * itself. Any other URL gets a `sourceReference` for the `source` request.
   */
  dapSource(source: Debugger.Source): DebugProtocol.Source {
    const path = this.clientPath(source.url);
    if (path !== undefined) {
      return { name: basename(path), path };
    }
    return { name: source.url, sourceReference: this.reference(source) };
  }

  /** The text of the source with `reference`. */
  text(reference: number): string | undefined {
    return this.sourcesByReference.get(reference)?.text;
  }

  private reference(source: Debugger.Source): number {
    let reference = this.references.get(source.url);
    if (reference === undefined) {
      reference = this.nextReference++;
      this.references.set(source.url, reference);
      this.sourcesByReference.set(reference, source);
    }
    return reference;
  }

  private clientPath(url: string): string | undefined {
    const remembered = this.clientPaths.get(url);
    if (remembered !== undefined) {
      return remembered;
    }
    const path = urlPath(url);
    return path === undefined ? undefined : (this.mapRemote(path) ?? path);
  }

  private mapRemote(path: string): string | undefined {
    if (this.localRoot === undefined || this.remoteRoot === undefined) {
      return undefined;
    }
    if (path === this.remoteRoot || path.startsWith(`${this.remoteRoot}/`)) {
      return this.localRoot + path.slice(this.remoteRoot.length);
    }
    return undefined;
  }
}

/** The filesystem path a script URL names, or `undefined` for URLs such as `<eval>`. */
function urlPath(url: string): string | undefined {
  if (url.startsWith("file://")) {
    return decodeURIComponent(url.slice("file://".length));
  }
  if (url.startsWith("/") || /^[A-Za-z]:[\\/]/.test(url)) {
    return url;
  }
  return undefined;
}

function endsWithSegments(path: string, suffix: string): boolean {
  if (!path.endsWith(suffix)) {
    return false;
  }
  const boundary = path.length - suffix.length;
  return boundary === 0 || suffix.startsWith("/") || path[boundary - 1] === "/";
}

function basename(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1);
}

function trimTrailingSlash(path: string | undefined): string | undefined {
  return path?.replace(/\/+$/, "");
}
