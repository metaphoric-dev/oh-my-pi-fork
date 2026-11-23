/**
 * omp's credential store (`agent.db`: API keys, OAuth tokens) stays out of
 * reach of the agent's file tools. Its 0600 mode keeps other users out, not
 * this process: the tools run as the same user, so they refuse it by path.
 *
 * A target matches when the file an `open()` of it would act on is the store
 * or one of its SQLite sidecars. Symlinks are followed, dangling ones too (a
 * write creates their target); the deepest existing ancestor is realpathed;
 * case folds where the filesystem usually folds it; and an existing file that
 * shares the store's inode (a hard link, another mount) matches whatever it is
 * called.
 */
import * as fs from "node:fs/promises";
import * as path from "node:path";
import { ToolError } from "@oh-my-pi/pi-tui/tools/tool-errors";
import { getAgentDbPath } from "@oh-my-pi/pi-utils";

/** SQLite keeps rows in the database file and, between checkpoints, in its sidecars. */
const STORE_FILE_SUFFIXES = ["", "-wal", "-shm", "-journal"];
/** Symlink hops before giving up (Linux MAXSYMLINKS). */
const MAX_SYMLINK_HOPS = 40;
const CASE_INSENSITIVE = process.platform === "win32" || process.platform === "darwin";

const CREDENTIAL_STORE_REFUSAL = "Refused: this path is omp's credential store; tools cannot read or change it.";

interface StoreFiles {
	/** Comparison keys of every store file's open target. */
	keys: Set<string>;
	/** `dev:ino` of every store file that exists. */
	inodes: Set<string>;
}

function comparisonKey(target: string): string {
	return CASE_INSENSITIVE ? target.toLowerCase() : target;
}

/** `target` with its deepest existing ancestor realpathed and the missing tail re-appended. */
async function realThroughMissing(target: string): Promise<string> {
	const tail: string[] = [];
	let current = target;
	for (;;) {
		try {
			return path.join(await fs.realpath(current), ...tail);
		} catch {
			const parent = path.dirname(current);
			if (parent === current) return target;
			tail.unshift(path.basename(current));
			current = parent;
		}
	}
}

/** The file an `open()` of `target` acts on: every symlink followed, a dangling one included. */
async function openTarget(target: string): Promise<string> {
	let current = path.resolve(target);
	for (let hop = 0; hop < MAX_SYMLINK_HOPS; hop++) {
		const located = path.join(await realThroughMissing(path.dirname(current)), path.basename(current));
		let link: string;
		try {
			link = await fs.readlink(located);
		} catch {
			return located;
		}
		current = path.resolve(path.dirname(located), link);
	}
	return current;
}

/** `dev:ino` of an existing regular file; `undefined` otherwise or when the filesystem reports no inode. */
async function fileIdentity(target: string): Promise<string | undefined> {
	try {
		const stat = await fs.stat(target, { bigint: true });
		return stat.isFile() && stat.ino !== 0n ? `${stat.dev}:${stat.ino}` : undefined;
	} catch {
		return undefined;
	}
}

/** The process's credential store and, when it differs, the one under the session's `agentDir`. */
async function storeFiles(agentDir: string | undefined): Promise<StoreFiles> {
	const databases = new Set([getAgentDbPath()]);
	if (agentDir !== undefined) databases.add(getAgentDbPath(agentDir));
	const keys = new Set<string>();
	const inodes = new Set<string>();
	for (const database of databases) {
		// SQLite names sidecars after the path it opened and after the real file a symlink names.
		const realDatabase = await openTarget(database);
		for (const suffix of STORE_FILE_SUFFIXES) {
			for (const file of new Set([`${database}${suffix}`, `${realDatabase}${suffix}`])) {
				const target = await openTarget(file);
				keys.add(comparisonKey(target));
				const identity = await fileIdentity(target);
				if (identity !== undefined) inodes.add(identity);
			}
		}
	}
	return { keys, inodes };
}

/**
 * Throw the model-facing refusal when `target` reaches omp's credential store.
 * The refusal echoes neither the resolved path nor anything stored inside it.
 *
 * @param target Host path a tool is about to read, search, or write; resolved against the process cwd when relative.
 * @param agentDir The session's agent directory, whose store is refused besides the process default.
 */
export async function assertNotCredentialStore(target: string, agentDir?: string): Promise<void> {
	const store = await storeFiles(agentDir);
	const destination = await openTarget(target);
	if (store.keys.has(comparisonKey(destination))) throw new ToolError(CREDENTIAL_STORE_REFUSAL);
	const identity = await fileIdentity(destination);
	if (identity !== undefined && store.inodes.has(identity)) throw new ToolError(CREDENTIAL_STORE_REFUSAL);
}
