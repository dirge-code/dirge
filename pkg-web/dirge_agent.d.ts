/* tslint:disable */
/* eslint-disable */
/**
 * The `ReadableStreamType` enum.
 *
 * *This API requires the following crate features to be activated: `ReadableStreamType`*
 */

export type ReadableStreamType = "bytes";

/**
 * A stateful wasm agent the host extends with JS-implemented tools. Build one,
 * register tools with `add_js_tool`, then `run` a prompt; the DeepSeek-backed
 * loop dispatches to whichever tools the model calls.
 */
export class AgentHandle {
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Register the built-in `EchoTool` so the agent has a tool without the
     * host supplying any JS.
     */
    add_echo_tool(): void;
    /**
     * Register a JS-implemented tool. `parameters` is a JSON Schema string;
     * `func` is `(argsJson: string) => string | Promise<string>`.
     */
    add_js_tool(name: string, description: string, parameters: string, func: Function): void;
    /**
     * Dispatch a registered tool directly (without the model). Exposes the JS
     * tool round-trip for tests and host-side use.
     */
    call_tool(name: string, args: string): Promise<string>;
    constructor(api_key: string);
    /**
     * Run one user turn with the current tool set over DeepSeek, returning the
     * final assistant text.
     */
    run(prompt: string): Promise<string>;
}

export class IntoUnderlyingByteSource {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    cancel(): void;
    pull(controller: ReadableByteStreamController): Promise<any>;
    start(controller: ReadableByteStreamController): void;
    readonly autoAllocateChunkSize: number;
    readonly type: ReadableStreamType;
}

export class IntoUnderlyingSink {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    abort(reason: any): Promise<any>;
    close(): Promise<any>;
    write(chunk: any): Promise<any>;
}

export class IntoUnderlyingSource {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    cancel(): void;
    pull(controller: ReadableStreamDefaultController): Promise<any>;
}

/**
 * In-memory session store exposed to JS. A pared-down, JSON-backed mirror of
 * the terminal session persistence: create a session, append messages, then
 * load / list / delete. Backed by [`crate::session_core::MemorySessionStore`].
 */
export class SessionStore {
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Append a message to a session. `role` is "user" | "assistant" | "system".
     */
    append_message(id: string, role: string, content: string): void;
    /**
     * Create a new session and return its id.
     */
    create(name: string): string;
    /**
     * Delete a session (idempotent).
     */
    delete(id: string): void;
    /**
     * Load one session, returned as a JSON object string.
     */
    get(id: string): string;
    /**
     * List all sessions as a JSON array string, newest first.
     */
    list(): string;
    constructor();
}

/**
 * BYOK agent turn over DeepSeek with tool-calling. Builds the minimal
 * [`crate::agent_core::Agent`] loop with the built-in `EchoTool` and returns
 * the final assistant text as a JS Promise. The tool set is swappable; this
 * export is the seed for a JS-registered tool registry (shell/fs/process
 * shims supplied by the host).
 */
export function agent_chat(api_key: string, prompt: string): Promise<string>;

/**
 * BYOK chat completion over DeepSeek. Takes a caller-supplied API key so no
 * credential leaves the JS side (browser BYOK), then runs dirge's rig-based
 * DeepSeek provider over the wasm `fetch` transport. Returns the assistant
 * text as a JS Promise.
 */
export function chat(api_key: string, prompt: string): Promise<string>;

/**
 * Approximate token count for a text string, using dirge's llmtrim
 * BPE-shaped estimator (the same counter used for Anthropic/Google requests
 * on the native side). Returns the token count as a JS number.
 */
export function token_count(text: string): number;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_agenthandle_free: (a: number, b: number) => void;
    readonly __wbg_sessionstore_free: (a: number, b: number) => void;
    readonly agent_chat: (a: number, b: number, c: number, d: number) => any;
    readonly agenthandle_add_echo_tool: (a: number) => void;
    readonly agenthandle_add_js_tool: (a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: any) => [number, number];
    readonly agenthandle_call_tool: (a: number, b: number, c: number, d: number, e: number) => any;
    readonly agenthandle_new: (a: number, b: number) => number;
    readonly agenthandle_run: (a: number, b: number, c: number) => any;
    readonly chat: (a: number, b: number, c: number, d: number) => any;
    readonly sessionstore_append_message: (a: number, b: number, c: number, d: number, e: number, f: number, g: number) => [number, number];
    readonly sessionstore_create: (a: number, b: number, c: number) => [number, number];
    readonly sessionstore_delete: (a: number, b: number, c: number) => [number, number];
    readonly sessionstore_get: (a: number, b: number, c: number) => [number, number, number, number];
    readonly sessionstore_list: (a: number) => [number, number];
    readonly sessionstore_new: () => number;
    readonly token_count: (a: number, b: number) => number;
    readonly __wbg_intounderlyingbytesource_free: (a: number, b: number) => void;
    readonly intounderlyingbytesource_autoAllocateChunkSize: (a: number) => number;
    readonly intounderlyingbytesource_cancel: (a: number) => void;
    readonly intounderlyingbytesource_pull: (a: number, b: any) => any;
    readonly intounderlyingbytesource_start: (a: number, b: any) => void;
    readonly intounderlyingbytesource_type: (a: number) => number;
    readonly __wbg_intounderlyingsource_free: (a: number, b: number) => void;
    readonly intounderlyingsource_cancel: (a: number) => void;
    readonly intounderlyingsource_pull: (a: number, b: any) => any;
    readonly __wbg_intounderlyingsink_free: (a: number, b: number) => void;
    readonly intounderlyingsink_abort: (a: number, b: any) => any;
    readonly intounderlyingsink_close: (a: number) => any;
    readonly intounderlyingsink_write: (a: number, b: any) => any;
    readonly wasm_bindgen__convert__closures_____invoke__h1214871e6dac7a9b: (a: number, b: number, c: any) => [number, number];
    readonly wasm_bindgen__convert__closures_____invoke__h5a7dad4203aed81f: (a: number, b: number, c: any, d: any) => void;
    readonly wasm_bindgen__convert__closures_____invoke__heffd7ef854a7947f: (a: number, b: number) => number;
    readonly wasm_bindgen__convert__closures_____invoke__h4389e72fa9435afd: (a: number, b: number) => void;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_destroy_closure: (a: number, b: number) => void;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
