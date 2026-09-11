const tauri = window["__TAURI__"];
const { invoke } = tauri.core;
const { listen } = tauri.event;

const elements = {
  activity: document.querySelector("#activity"),
  activityLabel: document.querySelector("#activity-label"),
  actionControls: document.querySelector("#action-controls"),
  channel: document.querySelector("#channel"),
  connectionChoices: document.querySelector("#connection-choices"),
  connectionLocal: document.querySelector("#connection-local"),
  connectionRemote: document.querySelector("#connection-remote"),
  description: document.querySelector("#description"),
  discovery: document.querySelector("#discovery"),
  eyebrow: document.querySelector("#eyebrow"),
  footerMode: document.querySelector("#footer-mode"),
  gatewayList: document.querySelector("#gateway-list"),
  discoveryStatus: document.querySelector("#discovery-status"),
  installButton: document.querySelector("#install-button"),
  installControls: document.querySelector("#install-controls"),
  installLog: document.querySelector("#install-log"),
  logStatus: document.querySelector("#log-status"),
  logWrap: document.querySelector("#log-wrap"),
  primaryAction: document.querySelector("#primary-action"),
  recoveryLocal: document.querySelector("#recovery-local"),
  recoveryNote: document.querySelector("#recovery-note"),
  recoveryPanel: document.querySelector("#recovery-panel"),
  recoveryRemote: document.querySelector("#recovery-remote"),
  remoteAuth: document.querySelector(".remote-auth"),
  remoteConnect: document.querySelector("#remote-connect"),
  remoteDetails: document.querySelector("#remote-details"),
  remoteFeedback: document.querySelector("#remote-feedback"),
  remotePassword: document.querySelector("#remote-password"),
  remotePort: document.querySelector("#remote-port"),
  remoteSshField: document.querySelector("#remote-ssh-field"),
  remoteSshTarget: document.querySelector("#remote-ssh-target"),
  remoteSubtitle: document.querySelector("#remote-subtitle"),
  remoteToken: document.querySelector("#remote-token"),
  remoteTransportDirect: document.querySelector("#remote-transport-direct"),
  remoteTransportSsh: document.querySelector("#remote-transport-ssh"),
  remoteUrl: document.querySelector("#remote-url"),
  remoteUrlField: document.querySelector("#remote-url-field"),
  setupBack: document.querySelector("#setup-back"),
  setupContinue: document.querySelector("#setup-continue"),
  statusDot: document.querySelector("#status-dot"),
  title: document.querySelector("#title"),
  updateAction: document.querySelector("#update-action"),
  updateBanner: document.querySelector("#update-banner"),
  updateDismiss: document.querySelector("#update-dismiss"),
  updateMessage: document.querySelector("#update-message"),
  updateProgress: document.querySelector("#update-progress"),
  updateTitle: document.querySelector("#update-title"),
};

let primaryAction = null;
let updateAction = null;
let discoveryPending = false;
let discoverySignature = null;
let firstRunBuild = null;
let firstRunPhase = null;
// 首次运行那条路上的自动重试计时器：网关只是慢，不是坏，别让用户盯着一个静止的页。
let recoveryPoll = null;
let selectedConnection = "local";
let remoteTransport = "direct";
let remoteConnectionPending = false;

function show(element, visible) {
  element.classList.toggle("hidden", !visible);
}

function render({
  activity = null,
  description,
  dot = "working",
  eyebrow = "启动中",
  showDiscovery = false,
  showInstall = false,
  title,
}) {
  elements.eyebrow.textContent = eyebrow;
  elements.title.textContent = title;
  elements.description.textContent = description;
  elements.statusDot.className = `status-dot ${dot}`;
  show(elements.activity, Boolean(activity));
  if (activity) {
    elements.activityLabel.textContent = activity;
  }
  show(elements.installControls, showInstall);
  show(elements.actionControls, false);
  show(elements.recoveryPanel, false);
  show(elements.connectionChoices, false);
  show(elements.discovery, showDiscovery);
}

function renderAction(options, action) {
  render(options);
  primaryAction = action;
  elements.primaryAction.textContent = options.actionLabel;
  show(elements.actionControls, true);
}

function formatInstallLine(line) {
  let event;
  try {
    event = JSON.parse(line);
  } catch {
    return line;
  }
  if (!event || typeof event !== "object" || !event.event) {
    return line;
  }
  if (event.event === "done" && event.ok === true) {
    return `✓ 安装完成${event.version ? ` ${event.version}` : ""}`;
  }
  if (event.event !== "step" || !event.name) {
    return line;
  }

  const name =
    {
      node: "Node 运行时",
      git: "Git 检出",
      openclaw: "运行时命令行",
      "gateway-service": "网关服务",
      "control-ui": "控制台构建",
      "cli-build": "命令行构建",
    }[event.name] || event.name;
  switch (event.status) {
    case "start":
      return `→ ${name}${event.version ? ` ${event.version}` : ""}…`;
    case "ok":
      return `✓ ${name}`;
    case "skip":
      return `– 跳过 ${name}${event.reason ? `（${event.reason}）` : ""}`;
    case "warn":
      return `! ${name}${event.reason ? `: ${event.reason}` : ""}`;
    default:
      return line;
  }
}

function appendLog(line) {
  elements.installLog.textContent += `${formatInstallLine(line)}\n`;
  elements.installLog.scrollTop = elements.installLog.scrollHeight;
}

function renderUpdate({ action = null, actionLabel = "", message, progress = false, title }) {
  elements.updateTitle.textContent = title;
  elements.updateMessage.textContent = message;
  updateAction = action;
  elements.updateAction.textContent = actionLabel;
  show(elements.updateAction, Boolean(action));
  show(elements.updateProgress, progress);
  show(elements.updateBanner, true);
}

function formatBytes(bytes) {
  if (!Number.isFinite(bytes)) {
    return "";
  }
  const units = ["B", "KB", "MB", "GB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(unit === 0 ? 0 : 1)} ${units[unit]}`;
}

function friendlyError(error) {
  if (typeof error === "string") {
    return error;
  }
  return error?.message || "星舰没能完成这一步。";
}

function gatewayHost(gateway) {
  return (gateway.host || "").trim().replace(/\.$/, "");
}

function canConnectDirect(gateway) {
  return (
    gateway.tls ||
    gateway.directReachable ||
    gatewayHost(gateway).toLowerCase().endsWith(".ts.net")
  );
}

function renderGateways(gateways) {
  elements.gatewayList.replaceChildren();
  elements.discoveryStatus.textContent = gateways.length ? `找到 ${gateways.length} 个` : "搜索中";
  elements.remoteSubtitle.textContent = gateways.length
    ? `在本机网络上找到 ${gateways.length} 个网关。`
    : "连接到别处运行的网关。";
  if (!gateways.length) {
    const empty = document.createElement("p");
    empty.className = "discovery-empty";
    empty.textContent = "正在查找附近的网关…";
    elements.gatewayList.append(empty);
    return;
  }

  for (const gateway of gateways) {
    const button = document.createElement("button");
    button.className = "gateway-card";
    button.type = "button";
    button.disabled = !canConnectDirect(gateway);
    if (button.disabled) {
      button.title = "这个网关没有声明可直连。";
    }

    const copy = document.createElement("span");
    copy.className = "gateway-copy";
    const name = document.createElement("span");
    name.className = "gateway-name";
    name.textContent = gateway.name;
    const endpoint = document.createElement("span");
    endpoint.className = "gateway-endpoint";
    endpoint.textContent = `${gatewayHost(gateway)}:${gateway.port}`;
    copy.append(name, endpoint);

    const badge = document.createElement("span");
    badge.className = `gateway-badge${gateway.tls ? " secure" : ""}`;
    badge.textContent = gateway.tls ? "TLS" : "HTTP";
    button.append(copy, badge);
    button.addEventListener("click", () => {
      if (selectedConnection === "remote" && !elements.connectionChoices.classList.contains("hidden")) {
        const host = gatewayHost(gateway);
        const urlHost = host.includes(":") && !host.startsWith("[") ? `[${host}]` : host;
        selectRemoteTransport("direct");
        elements.remoteUrl.value = `${gateway.tls ? "https" : "http"}://${urlHost}:${gateway.port}`;
        void connectRemoteGateway();
        return;
      }
      button.disabled = true;
      void invoke("connect_discovered_gateway", {
        host: gateway.host,
        port: gateway.port,
        tls: gateway.tls,
      })
        .then(() => {
          button.disabled = false;
          elements.discoveryStatus.textContent = "已打开窗口";
        })
        .catch(() => {
          button.disabled = false;
          elements.discoveryStatus.textContent = "连接失败";
        });
    });
    elements.gatewayList.append(button);
  }
}

async function refreshGateways() {
  if (discoveryPending) {
    return;
  }
  discoveryPending = true;
  try {
    const gateways = await invoke("discover_gateways");
    const signature = JSON.stringify(gateways);
    if (signature !== discoverySignature) {
      discoverySignature = signature;
      renderGateways(gateways);
    }
  } catch {
    discoverySignature = null;
    elements.discoveryStatus.textContent = "不可用";
  } finally {
    discoveryPending = false;
  }
}

async function connect() {
  render({
    activity: "正在连接本地网关…",
    description: "星舰正在连接网关，连上会自动进入任务界面。",
    title: "正在启动星魂",
  });
  try {
    const snapshot = await invoke("bootstrap");
    if (snapshot.phase === "missingCli" || snapshot.phase === "unconfigured") {
      firstRunPhase = snapshot.phase;
      firstRunBuild = await invoke("build_info").catch(() => null);
      if (firstRunBuild?.releaseBuild === false) {
        elements.channel.value = "dev";
      }
      renderRecovery();
      startRecoveryPoll();
    }
  } catch (error) {
    renderRetry(friendlyError(error));
  }
}

// 冷启动就停在这里的原因只有两种：这台机器没装运行时，或者网关还没起来。
// 两种都自己先重试，用户看到的是一个「正在启动」的页，而不是一张问卷。
function stopRecoveryPoll() {
  if (!recoveryPoll) {
    return;
  }
  window.clearInterval(recoveryPoll);
  recoveryPoll = null;
}

function startRecoveryPoll() {
  if (recoveryPoll) {
    return;
  }
  let attempts = 0;
  recoveryPoll = window.setInterval(() => {
    attempts += 1;
    if (attempts > 24) {
      stopRecoveryPoll();
      elements.description.textContent =
        "自动重试已经停了。点下面的按钮继续：本机安装，或连接远程网关。";
      return;
    }
    if (!elements.recoveryPanel.classList.contains("hidden")) {
      elements.description.textContent = `正在等待网关…（已重试 ${attempts} 次）`;
    }
    void invoke("bootstrap")
      .then((snapshot) => {
        if (snapshot && snapshot.phase !== "missingCli" && snapshot.phase !== "unconfigured") {
          stopRecoveryPoll();
        }
      })
      .catch(() => {
        // 网关还没应答：下一轮再看，这里不打扰用户。
      });
  }, 5000);
}

function renderRecovery() {
  const missingRuntime = firstRunPhase === "missingCli";
  render({
    description: missingRuntime
      ? "这台电脑上还没有可用的运行时。装一次就好，之后启动不会再停在这一步。"
      : "网关还没就绪。星舰会继续自动重试，通常几秒钟就好。",
    dot: "idle",
    eyebrow: "首次运行",
    title: missingRuntime ? "先装一次运行时" : "正在等待网关",
  });
  elements.recoveryNote.textContent = missingRuntime
    ? "本机还没有可用的星舰运行时。装一次之后，以后启动都会直接进任务界面。"
    : "如果你本来就在本机跑网关，什么都不用做——连上会自动进入任务界面。";
  show(elements.recoveryPanel, true);
  show(elements.discovery, false);
}

function renderConnectionChoices() {
  stopRecoveryPoll();
  render({
    description: "大多数情况选本机。星舰会把运行时装好，并在后台把星魂跑起来。",
    dot: "idle",
    eyebrow: "连接方式",
    title: "星魂跑在哪台机器上？",
  });
  show(elements.connectionChoices, true);
  selectConnection(selectedConnection);
}

function selectConnection(connection) {
  selectedConnection = connection;
  const isRemote = connection === "remote";
  elements.connectionLocal.classList.toggle("selected", !isRemote);
  elements.connectionRemote.classList.toggle("selected", isRemote);
  elements.connectionLocal.setAttribute("aria-pressed", String(!isRemote));
  elements.connectionRemote.setAttribute("aria-pressed", String(isRemote));
  elements.footerMode.textContent = isRemote ? "远程网关" : "本地网关";
  show(elements.remoteDetails, isRemote);
  show(elements.discovery, isRemote);
  if (isRemote) {
    void refreshGateways();
  }
}

function selectRemoteTransport(transport) {
  remoteTransport = transport;
  const direct = transport === "direct";
  elements.remoteTransportDirect.classList.toggle("selected", direct);
  elements.remoteTransportSsh.classList.toggle("selected", !direct);
  elements.remoteTransportDirect.setAttribute("aria-pressed", String(direct));
  elements.remoteTransportSsh.setAttribute("aria-pressed", String(!direct));
  show(elements.remoteUrlField, direct);
  show(elements.remoteSshField, !direct);
  show(elements.remoteFeedback, false);
}

async function continueLocalSetup() {
  stopRecoveryPoll();
  if (firstRunPhase === "unconfigured") {
    render({
      activity: "正在启动本机网关…",
      description: "星舰正在这台电脑上把星魂准备好。",
      eyebrow: "首次运行",
      title: "正在准备星魂",
    });
    try {
      await invoke("bootstrap", { explicitLocal: true });
    } catch (error) {
      renderRetry(friendlyError(error));
    }
    return;
  }
  if (firstRunBuild?.releaseBuild === false) {
    render({
      description: "这是开发构建，最好装一个与它匹配的发布通道。",
      eyebrow: "首次运行",
      showInstall: true,
      title: "选择发布通道",
    });
    return;
  }
  await install();
}

async function connectRemoteGateway() {
  if (remoteConnectionPending) {
    return;
  }

  const isDirect = remoteTransport === "direct";
  if (elements.remoteToken.value && elements.remotePassword.value) {
    elements.remoteAuth.open = true;
    elements.remotePassword.setAttribute("aria-invalid", "true");
    elements.remotePassword.focus();
    showRemoteFeedback("token 和密码二选一，不要同时填。", true);
    return;
  }

  const endpoint = isDirect ? elements.remoteUrl : elements.remoteSshTarget;
  const endpointValue = endpoint.value.trim();
  if (!endpointValue) {
    endpoint.setAttribute("aria-invalid", "true");
    endpoint.focus();
    showRemoteFeedback(isDirect ? "先填网关地址。" : "先填 SSH 目标。", true);
    return;
  }

  const portValue = elements.remotePort.value.trim();
  const remotePort = portValue ? Number(portValue) : null;
  if (!isDirect && (!Number.isInteger(remotePort) || remotePort < 1 || remotePort > 65535)) {
    elements.remotePort.setAttribute("aria-invalid", "true");
    elements.remotePort.focus();
    showRemoteFeedback("网关端口要在 1 到 65535 之间。", true);
    return;
  }

  endpoint.removeAttribute("aria-invalid");
  elements.remotePort.removeAttribute("aria-invalid");
  remoteConnectionPending = true;
  elements.remoteConnect.disabled = true;
  elements.setupContinue.disabled = true;
  showRemoteFeedback("正在检查网关连接…", false);

  try {
    await invoke("connect_remote_gateway", {
      transport: remoteTransport,
      url: isDirect ? endpointValue : null,
      sshTarget: isDirect ? null : endpointValue,
      token: elements.remoteToken.value || null,
      password: elements.remotePassword.value || null,
      remotePort: isDirect ? null : remotePort,
    });
    showRemoteFeedback("网关已连接，正在打开星舰…", false);
  } catch (error) {
    const message = friendlyError(error);
    if (/auth|token|password|unauthori[sz]ed|forbidden|401|403/i.test(message)) {
      elements.remoteAuth.open = true;
    }
    showRemoteFeedback(message, true);
  } finally {
    remoteConnectionPending = false;
    elements.remoteConnect.disabled = false;
    elements.setupContinue.disabled = false;
  }
}

function showRemoteFeedback(message, isError) {
  elements.remoteFeedback.textContent = message;
  elements.remoteFeedback.classList.toggle("error", isError);
  show(elements.remoteFeedback, true);
}

async function install() {
  elements.installButton.disabled = true;
  elements.channel.disabled = true;
  elements.installLog.textContent = "";
  elements.logStatus.textContent = "运行中";
  show(elements.logWrap, true);
  render({
    activity: "正在安装运行时…",
    description: "正在把运行时和 Node 环境装到你的用户目录。",
    eyebrow: "安装中",
    title: "正在准备星魂",
  });
  try {
    await invoke("install_cli", { channel: elements.channel.value });
    elements.logStatus.textContent = "完成";
  } catch (error) {
    const message = friendlyError(error);
    elements.logStatus.textContent = "失败";
    appendLog(message);
    render({
      description: message,
      dot: "error",
      eyebrow: "安装问题",
      showInstall: true,
      title: "安装没走完",
    });
  } finally {
    elements.installButton.disabled = false;
    elements.channel.disabled = false;
  }
}

async function runGatewayAction(action) {
  render({
    activity: `${action === "restart" ? "正在重启" : "正在启动"}网关…`,
    description: "星舰正在等本地网关就绪。",
    eyebrow: "网关",
    title: "稍等一下",
  });
  try {
    await invoke("gateway_action", { action });
  } catch (error) {
    renderRetry(friendlyError(error));
  }
}

function renderRetry(message) {
  show(elements.logWrap, false);
  renderAction(
    {
      actionLabel: "重试",
      description: message,
      dot: "error",
      eyebrow: "连接问题",
      // A broken managed CLI can only be replaced by reinstalling; retry alone
      // must never be the sole exit from a connection failure.
      showInstall: true,
      title: "星舰需要处理一下",
    },
    connect,
  );
}

elements.installButton.addEventListener("click", () => {
  void install();
});
elements.recoveryLocal.addEventListener("click", () => {
  void continueLocalSetup();
});
elements.recoveryRemote.addEventListener("click", renderConnectionChoices);
elements.connectionLocal.addEventListener("click", () => selectConnection("local"));
elements.connectionRemote.addEventListener("click", () => selectConnection("remote"));
elements.setupBack.addEventListener("click", renderRecovery);
elements.setupContinue.addEventListener("click", () => {
  void (selectedConnection === "remote" ? connectRemoteGateway() : continueLocalSetup());
});
elements.remoteConnect.addEventListener("click", () => {
  void connectRemoteGateway();
});
elements.remoteTransportDirect.addEventListener("click", () => selectRemoteTransport("direct"));
elements.remoteTransportSsh.addEventListener("click", () => selectRemoteTransport("ssh"));
for (const input of [
  elements.remoteUrl,
  elements.remoteSshTarget,
  elements.remotePort,
  elements.remoteToken,
  elements.remotePassword,
]) {
  input.addEventListener("input", () => {
    input.removeAttribute("aria-invalid");
    show(elements.remoteFeedback, false);
  });
}
elements.primaryAction.addEventListener("click", () => {
  void primaryAction?.();
});
elements.updateAction.addEventListener("click", () => {
  void updateAction?.();
});
elements.updateDismiss.addEventListener("click", () => {
  show(elements.updateBanner, false);
});

await listen("install-progress", ({ payload }) => appendLog(payload.line));
await listen("updater://not-available", () => {
  renderUpdate({
    message: "当前就是最新版本。",
    title: "已是最新版本",
  });
});
await listen("updater://available", ({ payload }) => {
  elements.updateProgress.removeAttribute("value");
  renderUpdate({
    message: payload.notes || "正在后台下载…",
    progress: true,
    title: `发现新版本 v${payload.version} — 正在下载…`,
  });
});
await listen("updater://progress", ({ payload }) => {
  if (payload.total) {
    elements.updateProgress.max = payload.total;
    elements.updateProgress.value = payload.downloaded;
    elements.updateMessage.textContent = `${formatBytes(payload.downloaded)} / ${formatBytes(payload.total)}`;
  } else {
    elements.updateProgress.removeAttribute("value");
    elements.updateMessage.textContent = `已下载 ${formatBytes(payload.downloaded)}`;
  }
});
await listen("updater://ready", ({ payload }) => {
  renderUpdate({
    action: () => invoke("relaunch"),
    actionLabel: "重启完成更新",
    message: `v${payload.version} 已装好，重启即可生效。`,
    title: "更新已就绪",
  });
});
await listen("updater://available-manual", ({ payload }) => {
  renderUpdate({
    action: () =>
      invoke("open_release_page").catch((error) => {
        renderUpdate({
          message: friendlyError(error),
          title: "打不开下载页",
        });
      }),
    actionLabel: "打开下载页",
    message: payload.notes || "到下载页装最新安装包。",
    title: `发现新版本 v${payload.version}`,
  });
});
await listen("updater://error", ({ payload }) => {
  renderUpdate({
    message: payload.message,
    title: "检查更新失败",
  });
});
void invoke("updater_ready");
void refreshGateways();
window.setInterval(() => void refreshGateways(), 2000);

const mode = new URLSearchParams(window.location.search).get("mode");
if (mode === "missingCli") {
  render({
    description: "本机的运行时不见了。装一次就能连上本地网关。",
    dot: "idle",
    eyebrow: "缺少运行时",
    showInstall: true,
    title: "先装一次运行时",
  });
} else if (mode === "reconnecting") {
  render({
    activity: "每几秒重试一次…",
    description: "网关连接断了，星舰会自动恢复控制台。",
    eyebrow: "网关离线",
    title: "正在重新连接",
  });
} else if (mode === "stopped") {
  renderAction(
    {
      actionLabel: "启动网关",
      description: "网关已停止。星舰会留在托盘里。",
      dot: "idle",
      eyebrow: "网关已停止",
      title: "星舰在待命",
    },
    () => runGatewayAction("start"),
  );
} else if (mode === "error") {
  renderRetry("上一次网关操作失败了。检查服务后重试。");
} else {
  await connect();
}
