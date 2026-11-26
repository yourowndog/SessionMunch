// SessionMunch visible-turn collector (prototype, MUNCH-022).
//
// Runs inside a page the owner has enrolled and reads ONLY what is rendered
// as visible text for each message element a surface profile selects. It
// emits `sessionmunch.observed-visible-turn.v1` records for finalized turns.
//
// Fail-closed rules (mirroring `sessionmunch import chats --format observed`):
// - no native conversation UUID in the page URL  -> emit nothing
// - no native message UUID on a message element  -> emit nothing, count it
// - message still streaming                      -> emit nothing yet, count it
// - unknown role                                 -> emit nothing, count it
// It never reads cookies, storage, network responses or hidden DOM, and it
// never sends anything: records stay in memory until the local runner drains
// them.
(function install(global) {
  "use strict";

  const SCHEMA = "sessionmunch.observed-visible-turn.v1";
  const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

  function requireString(profile, key) {
    if (typeof profile[key] !== "string" || profile[key].length === 0) {
      throw new Error(`profile.${key} must be a non-empty string`);
    }
    return profile[key];
  }

  function createCollector(profile, options) {
    const provider = options && options.provider;
    if (provider !== "chatgpt" && provider !== "claude") {
      throw new Error("options.provider must be chatgpt or claude");
    }
    const surface = (options && options.surface) || "web";
    const messageSelector = requireString(profile, "messageSelector");
    const messageIdAttribute = requireString(profile, "messageIdAttribute");
    const roleAttribute = requireString(profile, "roleAttribute");
    const textSelector = profile.textSelector || null;
    const streamingSelector = profile.streamingSelector || null;
    const conversationPattern = new RegExp(requireString(profile, "conversationIdPattern"));
    const roles = profile.roles || { user: "user", assistant: "assistant" };

    const lastText = new Map();
    const pending = [];
    const counts = {
      emitted: 0,
      no_conversation_id: 0,
      no_message_id: 0,
      non_final: 0,
      unsupported_role: 0,
    };
    // Rescans run on every DOM mutation; count each element or message once.
    const seen = {
      no_conversation_id: new WeakSet(),
      no_message_id: new WeakSet(),
      non_final: new Set(),
      unsupported_role: new WeakSet(),
    };
    function countOnce(kind, key) {
      if (!seen[kind].has(key)) {
        seen[kind].add(key);
        counts[kind] += 1;
      }
    }

    function conversationId() {
      const match = conversationPattern.exec(global.location.href);
      const id = match && match[1];
      return id && UUID.test(id) ? id.toLowerCase() : null;
    }

    function visibleText(element) {
      const target = textSelector ? element.querySelector(textSelector) : element;
      // innerText follows rendering: display:none / hidden content is omitted.
      return target ? target.innerText.replace(/\s+$/u, "") : "";
    }

    function scan() {
      const conversation = conversationId();
      const elements = global.document.querySelectorAll(messageSelector);
      if (!conversation) {
        for (const element of elements) countOnce("no_conversation_id", element);
        return;
      }
      for (const element of elements) {
        const rawId = element.getAttribute(messageIdAttribute);
        if (!rawId || !UUID.test(rawId)) {
          countOnce("no_message_id", element);
          continue;
        }
        const role = roles[element.getAttribute(roleAttribute)];
        if (role !== "user" && role !== "assistant") {
          countOnce("unsupported_role", element);
          continue;
        }
        if (streamingSelector && (element.matches(streamingSelector) || element.querySelector(streamingSelector))) {
          countOnce("non_final", rawId.toLowerCase());
          continue;
        }
        const text = visibleText(element);
        if (text.trim().length === 0) {
          continue;
        }
        const messageId = rawId.toLowerCase();
        if (lastText.get(messageId) === text) {
          continue;
        }
        lastText.set(messageId, text);
        const now = new Date().toISOString();
        const record = {
          schema: SCHEMA,
          provider,
          surface,
          conversation_id: conversation,
          message_id: messageId,
          role,
          final: true,
          text,
          observed_at: now,
        };
        const occurred = profile.occurredAtAttribute && element.getAttribute(profile.occurredAtAttribute);
        if (occurred && !Number.isNaN(Date.parse(occurred))) {
          record.occurred_at = new Date(occurred).toISOString();
        }
        pending.push(record);
        counts.emitted += 1;
      }
    }

    let observer = null;
    return {
      scan,
      start() {
        scan();
        observer = new global.MutationObserver(() => scan());
        observer.observe(global.document.body, {
          subtree: true,
          childList: true,
          characterData: true,
          attributes: true,
        });
      },
      stop() {
        if (observer) observer.disconnect();
        observer = null;
      },
      drain() {
        return pending.splice(0, pending.length);
      },
      counts() {
        return { ...counts };
      },
    };
  }

  global.__sessionmunchVisibleTurnCollector = { createCollector, SCHEMA };
})(globalThis);
