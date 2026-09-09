// The CLI deliberately has no runtime dependencies. These small declarations
// let its package compile in a bare workspace; consumers run the emitted JS in
// Node 18+, where these built-ins are present.
declare module "node:fs/promises" {
  export interface Dirent { name: string; isFile(): boolean }
  export function readFile(path: string, encoding: "utf8"): Promise<string>;
  export function readdir(path: string, options: { withFileTypes: true }): Promise<Dirent[]>;
}

declare module "node:path" {
  export function resolve(...paths: string[]): string;
}

declare const process: {
  argv: string[];
  env: Record<string, string | undefined>;
  exitCode?: number;
};
