/**
 * The agent's file tools refuse omp's credential store (`agent.db` and its SQLite sidecars). The
 * store's 0600 mode only keeps other users out; the tools run as the same user, so a prompt-injected
 * call could otherwise read the API keys and OAuth tokens or replace the database.
 */
import { afterEach, beforeEach, describe, expect, it } from "bun:test";
import { Database } from "bun:sqlite";
import * as fs from "node:fs";
import * as path from "node:path";
import { Settings } from "@oh-my-pi/pi-coding-agent/config/settings";
import { EditTool } from "@oh-my-pi/pi-coding-agent/edit";
import type { ToolSession } from "@oh-my-pi/pi-coding-agent/tools";
import { GrepTool } from "@oh-my-pi/pi-coding-agent/tools/grep";
import { FindTool } from "@oh-my-pi/pi-coding-agent/tools/jfind";
import { ReadTool } from "@oh-my-pi/pi-coding-agent/tools/read";
import { WriteTool } from "@oh-my-pi/pi-coding-agent/tools/write";
import { getAgentDbPath, getAgentDir, setAgentDir, TempDir } from "@oh-my-pi/pi-utils";

const SECRET = "sk-ant-api03-credential-store-sentinel";
const REFUSAL = "omp's credential store";

function makeSession(cwd: string): ToolSession {
	return {
		cwd,
		hasUI: false,
		enableLsp: false,
		getSessionFile: () => null,
		getSessionSpawns: () => "*",
		getArtifactsDir: () => null,
		getSessionId: () => null,
		getPlanModeState: () => undefined,
		settings: Settings.isolated({ "edit.mode": "patch" }),
	} as ToolSession;
}

/** A credential store the way omp lays it out: `agent.db` under the agent directory, mode 0600. */
function createStore(): string {
	const dbPath = getAgentDbPath();
	fs.mkdirSync(path.dirname(dbPath), { recursive: true, mode: 0o700 });
	const db = new Database(dbPath);
	try {
		db.run("CREATE TABLE auth_credentials (provider TEXT NOT NULL, data TEXT NOT NULL)");
		db.run("INSERT INTO auth_credentials (provider, data) VALUES (?, ?)", [
			"anthropic",
			JSON.stringify({ type: "api_key", key: SECRET }),
		]);
	} finally {
		db.close();
	}
	fs.chmodSync(dbPath, 0o600);
	return dbPath;
}

describe.skipIf(process.platform === "win32")("file tools refuse omp's credential store", () => {
	let temp: TempDir;
	let cwd: string;
	let store: string;
	let storeBytes: Buffer;
	let originalAgentDir: string;

	beforeEach(() => {
		temp = TempDir.createSync("@omp-credential-store-");
		cwd = temp.join("workspace");
		fs.mkdirSync(cwd);
		originalAgentDir = getAgentDir();
		setAgentDir(temp.join("agent"));
		store = createStore();
		storeBytes = fs.readFileSync(store);
	});

	afterEach(() => {
		setAgentDir(originalAgentDir);
		temp.removeSync();
	});

	function expectStoreUntouched(): void {
		expect(fs.readFileSync(store).equals(storeBytes)).toBe(true);
	}

	it("read refuses the store's own path instead of returning a credential row", async () => {
		const read = new ReadTool(makeSession(cwd)).execute("read-store", { path: `${store}:auth_credentials:1` });
		await expect(read).rejects.toThrow(REFUSAL);
	});

	it("read refuses a workspace symlink that points at the store", async () => {
		fs.symlinkSync(store, path.join(cwd, "fixtures.db"));
		const read = new ReadTool(makeSession(cwd)).execute("read-link", { path: "fixtures.db:auth_credentials:1" });
		await expect(read).rejects.toThrow(REFUSAL);
	});

	it("read refuses a hard link to the store, which no path comparison can see", async () => {
		fs.linkSync(store, path.join(cwd, "backup.db"));
		const read = new ReadTool(makeSession(cwd)).execute("read-hardlink", { path: "backup.db:auth_credentials:1" });
		await expect(read).rejects.toThrow(REFUSAL);
	});

	it("write refuses to replace the store", async () => {
		const write = new WriteTool(makeSession(cwd)).execute("write-store", { path: store, content: "replaced\n" });
		await expect(write).rejects.toThrow(REFUSAL);
		expectStoreUntouched();
	});

	// SQLite replays a hot `-journal` into the database on its next open, so creating one rewrites the store.
	it("write refuses to create a sidecar the store does not have yet", async () => {
		const journal = `${store}-journal`;
		const write = new WriteTool(makeSession(cwd)).execute("write-journal", { path: journal, content: "x" });
		await expect(write).rejects.toThrow(REFUSAL);
		expect(fs.existsSync(journal)).toBe(false);
	});

	it("write refuses a dangling workspace symlink whose target would be a store sidecar", async () => {
		const journal = `${store}-journal`;
		fs.symlinkSync(journal, path.join(cwd, "notes.txt"));
		const write = new WriteTool(makeSession(cwd)).execute("write-dangling", { path: "notes.txt", content: "x" });
		await expect(write).rejects.toThrow(REFUSAL);
		expect(fs.existsSync(journal)).toBe(false);
	});

	it("edit refuses to delete the store", async () => {
		const edit = new EditTool(makeSession(cwd), "patch").execute("edit-store", {
			path: store,
			edits: [{ op: "delete" }],
		});
		await expect(edit).rejects.toThrow(REFUSAL);
		expectStoreUntouched();
	});

	it("grep refuses the store as a search root", async () => {
		const grep = new GrepTool(makeSession(cwd)).execute("grep-store", { pattern: "sk-ant", path: store });
		await expect(grep).rejects.toThrow(REFUSAL);
	});

	it("find refuses the store as a search root", async () => {
		const find = new FindTool(makeSession(cwd)).execute("find-store", {
			query: "api key",
			grep_keywords: ["sk-ant"],
			path: store,
		});
		await expect(find).rejects.toThrow(REFUSAL);
	});

	it("read still serves an ordinary SQLite file beside the store", async () => {
		const other = path.join(path.dirname(store), "notes.db");
		const db = new Database(other);
		db.run("CREATE TABLE notes (body TEXT)");
		db.run("INSERT INTO notes (body) VALUES ('kept')");
		db.close();
		const result = await new ReadTool(makeSession(cwd)).execute("read-other", { path: `${other}:notes` });
		const text = result.content.map(part => (part.type === "text" ? part.text : "")).join("\n");
		expect(text).toContain("kept");
	});
});
