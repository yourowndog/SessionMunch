
// `Plugin.define` is an identity wrapper, so the binding exports the
// `{ id, setup }` shape directly and keeps the shared `import type` line:
// a runtime import of `@opencode-ai/plugin` does not resolve from the
// global plugins dir and fails the load.
const SessionMunchOpencode2: Plugin = {
  id: "sessionmunch-opencode2",
  setup: async (ctx) => {
    const ctxAny = ctx as any;
    const directory = ctxAny?.location?.directory;
    const controller = new AbortController();
    void (async () => {
      try {
        for await (const evt of ctx.event.subscribe({ signal: controller.signal })) {
          const event = evt as any;
          const type = event?.type;
          const data = event?.data ?? event?.properties ?? {};
          const info = data?.info ?? {};
          const loc = data?.location?.directory ?? data?.directory
            ?? event?.location?.directory ?? directory;
          if (type === "session.created") {
            const id = data?.sessionID ?? data?.id ?? info?.id;
            startSession(id, data?.location?.directory ?? loc, {
              title: data?.title ?? info?.title,
              projectID: data?.projectID ?? info?.projectID,
            });
          }
          if (type === "session.idle") {
            const id = data?.sessionID ?? data?.id;
            startSession(id, cwdFor(id, loc));
            postHook("stop", { sessionID: id, cwd: cwdFor(id, loc) });
          }
          if (type === "session.deleted") {
            const id = data?.sessionID ?? data?.id ?? info?.id;
            endSession(id, loc, data?.directory ?? info?.directory);
          }
          if (type === "session.compaction.started") {
            const id = data?.sessionID ?? data?.id;
            postPreCompact(id, loc);
          }
          if (type === "session.compacted") {
            const id = data?.sessionID ?? data?.id;
            postPreCompact(id, loc);
          }
        }
      } catch (_e) {
        // The stream ends on unload. Capture is best-effort and must never
        // break the host.
      }
    })();
    const promptRegistration = await ctx.session.hook("prompt", (event) => {
      const e = event as any;
      const id = e?.sessionID;
      const prompt = e?.prompt ?? {};
      const cwd = cwdFor(id, directory);
      startSession(id, cwd, { agent: prompt?.agent, model: prompt?.model });
      postHook("user-prompt", {
        sessionID: id,
        cwd,
        agent: prompt?.agent,
        model: prompt?.model,
        messageID: e?.messageID,
        prompt: typeof prompt?.text === "string" ? prompt.text : textFromParts(prompt?.parts),
      });
    });
    const handoffInjected = new Set<string>();
    const contextRegistration = await ctx.session.hook("context", async (event) => {
      const e = event as any;
      const id = e?.sessionID;
      if (!id || handoffInjected.has(id)) return;
      startSession(id, cwdFor(id, directory));
      let pending = handoffFetches.get(id);
      if (!pending) {
        pending = fetchHandoff(cwdFor(id, directory), id);
        handoffFetches.set(id, pending);
      }
      const handoff = await pending;
      if (handoff) {
        // SystemPart is `{ type: "text", text }` — the `type` discriminator
        // is required: without it the host fails the model call with a
        // schema validation error (observed live on beta-18999).
        e.system.push({ type: "text", text: handoff });
        handoffInjected.add(id);
      }
    });
    const beforeRegistration = await ctx.tool.hook("execute.before", (event) => {
      const e = event as any;
      const id = e?.sessionID;
      startSession(id, cwdFor(id, directory));
      postHook("pre-tool-use", {
        sessionID: id,
        cwd: cwdFor(id, directory),
        tool: e?.tool,
        callID: e?.id,
        args: e?.input,
      });
    });
    const afterRegistration = await ctx.tool.hook("execute.after", (event) => {
      const e = event as any;
      const id = e?.sessionID;
      startSession(id, cwdFor(id, directory));
      // The beta reports failures on the same channel (`status: "error"`,
      // no `result`); keep the message where v1 kept output so the
      // failure reason survives consolidation.
      const failed = e?.status === "error";
      postHook("post-tool-use", {
        sessionID: id,
        cwd: cwdFor(id, directory),
        tool: e?.tool,
        callID: e?.id,
        args: e?.input,
        title: failed ? undefined : e?.result?.title,
        output: failed
          ? String(e?.error?.message ?? e?.error ?? "tool failed")
          : e?.result?.output,
        metadata: failed ? undefined : e?.result?.metadata,
      });
    });
    return async () => {
      controller.abort();
      for (const registration of [
        promptRegistration,
        contextRegistration,
        beforeRegistration,
        afterRegistration,
      ]) {
        try {
          await registration.dispose();
        } catch (_e) {
          // Unload is best-effort; a dead host has nothing to unregister.
        }
      }
      for (const id of Array.from(startedSessions)) {
        endSession(id, directory);
      }
      await drainHookQueueForDispose();
    };
  },
};

export default SessionMunchOpencode2;
