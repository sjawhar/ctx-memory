/**
 * The ingest cycle's only way to write: two omp tools that hand records to `ctx record put` on
 * standard input and remove one with `ctx record rm`, so the session needs no tool that writes a
 * file. run-cycle.sh loads this module with --trusted-extension from /opt/ctx-memory, which is
 * root's. That flag makes omp load this module and discover no other extension; it does not stop
 * omp discovering custom tools or MCP servers, which the image closes by keeping every place they
 * are read from root's (docs/deployment.md, checked by run-cycle.sh). No shell runs: the
 * arguments are a fixed argv with the record's path after `--`, so nothing in a path or a record
 * is read as an option or as shell. Several records go together as one JSON list to
 * `ctx record put --batch -`, which the server checks against the tree with all of them in it
 * and commits in one commit, so new records that link each other can be written at all.
 */
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

async function ctx(args: string[], input: string | undefined, signal: AbortSignal | undefined) {
	const child = Bun.spawn(["ctx", ...args], {
		stdin: input === undefined ? "ignore" : new TextEncoder().encode(input),
		stdout: "pipe",
		stderr: "pipe",
		signal,
	});
	const [stdout, stderr, code] = await Promise.all([
		new Response(child.stdout).text(),
		new Response(child.stderr).text(),
		child.exited,
	]);
	const text = [stdout.trim(), stderr.trim()].filter(Boolean).join("\n");
	// A refused write exits non-zero and prints its findings; the call fails with them.
	if (code !== 0) throw new Error(text || `ctx ${args.slice(0, 2).join(" ")} exited ${code}`);
	return { content: [{ type: "text" as const, text: text || "ok" }], details: { exitCode: code } };
}

export default function (pi: ExtensionAPI) {
	const z = pi.zod;
	pi.registerTool({
		name: "ctx_record_put",
		label: "Record put",
		description:
			"Write records to the graph through the ctx server, each whole: one record, or several new " +
			"records that link each other, which the server checks together and commits in one commit. " +
			"A refused write fails with its findings and changes nothing, for every record it holds.",
		parameters: z.object({
			records: z
				.array(
					z.object({
						path: z.string().describe("Tree-relative path of the record, such as decisions/renewal.md"),
						content: z.string().describe("The whole record: frontmatter and body"),
					}),
				)
				.min(1)
				.describe("The records to write in one commit: usually one"),
		}),
		approval: "write",
		loadMode: "essential",
		execute: (_id, params, signal) => {
			const [only, ...rest] = params.records;
			if (only === undefined) throw new Error("ctx_record_put needs at least one record");
			if (rest.length === 0) return ctx(["record", "put", "--", only.path, "-"], only.content, signal);
			const batch = params.records.map(({ path, content }) => ({ path, content }));
			return ctx(["record", "put", "--batch", "-"], JSON.stringify(batch), signal);
		},
	});
	pi.registerTool({
		name: "ctx_record_rm",
		label: "Record rm",
		description: "Remove one record from the graph through the ctx server: `ctx record rm <path>`.",
		parameters: z.object({
			path: z.string().describe("Tree-relative path of the record"),
		}),
		approval: "write",
		loadMode: "essential",
		execute: (_id, params, signal) => ctx(["record", "rm", "--", params.path], undefined, signal),
	});
}
