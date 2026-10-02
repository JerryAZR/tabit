/**
 * The one tool-card registration site. Adding a tool card = one file here
 * that calls `registerToolCardRenderer` (its own file, its own summary /
 * expanded views), plus an import below — nothing else changes. Tools
 * without a registration render through the default handlers.
 *
 * Import order is the registration: each import self-registers.
 */

import "./edit.ts";
import "./read.ts";
import "./write.ts";
import "./bash.ts";

export { rendererFor, defaultCall, defaultResult } from "./registry.ts";
export type { ToolCardRenderer, ToolRenderInput } from "./registry.ts";
