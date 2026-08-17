var __defProp = Object.defineProperty;
var __name = (target, value) => __defProp(target, "name", { value, configurable: true });

// src/index.ts
import { Service } from "cordis";
import { Binary, defineProperty, isNullable } from "cosmokit";
import { createRequire } from "node:module";
import fetchFile from "@cordisjs/fetch-file";
import z from "schemastery";
var kHttpError = /* @__PURE__ */ Symbol.for("cordis.http.error");
var kHttpConfig = /* @__PURE__ */ Symbol.for("cordis.http.config");
var HttpError = class extends Error {
  constructor(message, code, response) {
    super(message);
    this.code = code;
    this.response = response;
  }
  code;
  response;
  static {
    __name(this, "HttpError");
  }
  [kHttpError] = true;
  static is(error) {
    return !!error?.[kHttpError];
  }
};
function encodeRequest(data) {
  if (data instanceof URLSearchParams) return [null, data];
  if (data instanceof ArrayBuffer) return [null, data];
  if (ArrayBuffer.isView(data)) return [null, data];
  if (data instanceof Blob) return [null, data];
  if (data instanceof FormData) return [null, data];
  if (data instanceof ReadableStream) return [null, data];
  return ["application/json", JSON.stringify(data)];
}
__name(encodeRequest, "encodeRequest");
var validateStatus = /* @__PURE__ */ __name((status) => status < 400, "validateStatus");
var Http = class _Http extends Service {
  constructor(ctx, config = {}) {
    super(ctx, "http");
    this.config = config;
    this.decoder("json", (raw) => raw.json());
    this.decoder("text", (raw) => raw.text());
    this.decoder("blob", (raw) => raw.blob());
    this.decoder("arraybuffer", (raw) => raw.arrayBuffer());
    this.decoder("formdata", (raw) => raw.formData());
    this.decoder("stream", (raw) => raw.body);
    this.decoder("headers", (raw) => raw.headers);
    this.proxy(["http", "https"], (url) => {
      return new this.undici.ProxyAgent(url.href);
    });
    this.ctx.on("http/fetch", async (url, init, config2, next) => {
      if (url.protocol !== "file:") return next();
      if (init.method !== "GET") {
        return new Response(null, { status: 405, statusText: "Method Not Allowed" });
      }
      return fetchFile(url, init, {
        download: true,
        onError: ctx.logger.error
      });
    }, { prepend: true });
    this.ctx.on("http/fetch", async (url, init, config2, next) => {
      const capture = /^data:([^,]*),(.*)$/.exec(url.href);
      if (!capture) return next();
      if (init.method !== "GET") {
        return new Response(null, { status: 405, statusText: "Method Not Allowed" });
      }
      let [, type, data] = capture;
      let bodyInit = data;
      if (type.endsWith(";base64")) {
        type = type.slice(0, -7);
        bodyInit = Binary.fromBase64(data);
      } else {
        bodyInit = decodeURIComponent(data);
      }
      return new Response(bodyInit, {
        status: 200,
        statusText: "OK",
        headers: { "content-type": type }
      });
    }, { prepend: true });
  }
  config;
  static {
    __name(this, "Http");
  }
  static Error = HttpError;
  static undici;
  static {
    const require2 = createRequire(import.meta.url);
    try {
      if (process.execArgv.includes("--expose-internals")) {
        this.undici = require2("internal/deps/undici/undici");
      } else {
        this.undici = require2("undici");
      }
    } catch {
    }
    for (const method of ["get", "delete"]) {
      defineProperty(_Http.prototype, method, async function(url, config) {
        const response = await this(url, { method, validateStatus, ...config });
        return this._decode(response);
      });
    }
    for (const method of ["patch", "post", "put"]) {
      defineProperty(_Http.prototype, method, async function(url, data, config) {
        const response = await this(url, { method, data, validateStatus, ...config });
        return this._decode(response);
      });
    }
  }
  static Config = z.object({
    timeout: z.natural().role("ms").description("等待请求的最长时间。"),
    keepAlive: z.boolean().description("是否保持连接。"),
    proxyAgent: z.string().description("代理服务器地址。")
  });
  Config = z.object({
    baseUrl: z.string().description("基础 URL。"),
    timeout: z.natural().role("ms").description("等待请求的最长时间。"),
    keepAlive: z.boolean().description("是否保持连接。"),
    proxyAgent: z.string().description("代理服务器地址。")
  });
  isError = HttpError.is;
  _decoders = /* @__PURE__ */ Object.create(null);
  _proxies = /* @__PURE__ */ Object.create(null);
  get undici() {
    if (_Http.undici) return _Http.undici;
    throw new Error("please install `undici`");
  }
  static mergeConfig = /* @__PURE__ */ __name((target, source) => ({
    ...target,
    ...source,
    headers: {
      ...target?.headers,
      ...source?.headers
    }
  }), "mergeConfig");
  decoder(type, decoder) {
    return this.ctx.effect(() => {
      this._decoders[type] = decoder;
      return () => delete this._decoders[type];
    }, "ctx.http.decoder()");
  }
  proxy(name, factory) {
    return this.ctx.effect(() => {
      for (const key of name) {
        this._proxies[key] = factory;
      }
      return () => {
        for (const key of name) {
          delete this._proxies[key];
        }
      };
    }, "ctx.http.proxy()");
  }
  extend(config = {}) {
    return this[Service.extend]({
      config: _Http.mergeConfig(this.config, config)
    });
  }
  resolveConfig(init) {
    return this[Service.resolveConfig](this.config, init);
  }
  resolveURL(url, config, isWebSocket = false) {
    try {
      url = new URL(url, config.baseUrl);
    } catch (error) {
      throw new TypeError(`Invalid URL: ${url}`);
    }
    if (isWebSocket) url.protocol = url.protocol.replace(/^http/, "ws");
    for (const [key, value] of Object.entries(config.params ?? {})) {
      if (isNullable(value)) continue;
      url.searchParams.append(key, value);
    }
    return url;
  }
  defaultDecoder(response) {
    const type = response.headers.get("content-type");
    if (type?.startsWith("application/json")) {
      return response.json();
    } else if (type?.startsWith("text/")) {
      return response.text();
    } else {
      return response.arrayBuffer();
    }
  }
  async [Service.invoke](...args) {
    let method;
    if (typeof args[1] === "string" || args[1] instanceof URL) {
      method = args.shift();
    }
    const config = this.resolveConfig(args[1]);
    const url = this.resolveURL(args[0], config);
    method ??= config.method ?? "GET";
    const controller = new AbortController();
    if (config.signal) {
      if (config.signal.aborted) {
        throw config.signal.reason;
      }
      config.signal.addEventListener("abort", () => {
        controller.abort(config.signal.reason);
      });
    }
    const dispose = this.ctx.effect(() => {
      const timer = config.timeout && setTimeout(() => {
        controller.abort(new HttpError("request timeout", "TIMEOUT"));
      }, config.timeout);
      return () => {
        clearTimeout(timer);
      };
    });
    controller.signal.addEventListener("abort", () => dispose());
    try {
      const headers = new Headers(config.headers);
      const init = {
        method,
        headers,
        body: config.data,
        keepalive: config.keepAlive,
        redirect: config.redirect,
        signal: controller.signal
      };
      if (config.data && typeof config.data === "object") {
        const [type, body] = encodeRequest(config.data);
        init.body = body;
        if (type && !headers.has("content-type")) {
          headers.append("content-type", type);
        }
      }
      if (init.body) {
        init.duplex = "half";
      }
      if (config.proxyAgent) {
        const proxyURL = new URL(config.proxyAgent);
        const factory = this._proxies[proxyURL.protocol.slice(0, -1)];
        if (!factory) throw new Error(`Cannot resolve proxy agent ${proxyURL}`);
        init.dispatcher = factory(proxyURL);
      }
      const response = await this.ctx.waterfall(this, "http/fetch", url, init, config, () => {
        this.ctx.logger("http:request").debug("%C %s", method, url.href);
        return this.undici.fetch(url, init);
      }).catch((cause) => {
        this.ctx.logger("http:request").debug("%C %s failed: %o", method, url.href, cause);
        if (_Http.Error.is(cause)) throw cause;
        const error = new _Http.Error(`fetch ${url} failed`);
        error.cause = cause;
        throw error;
      });
      this.ctx.logger("http:response").debug("%C %s %s %s", method, url.href, response.status, response.statusText);
      response[kHttpConfig] = config;
      return response;
    } finally {
      dispose();
    }
  }
  async _decode(response) {
    const config = response[kHttpConfig];
    const validateStatus2 = config.validateStatus ?? (() => true);
    if (!validateStatus2(response.status)) {
      throw new _Http.Error(response.statusText, "STATUS_ERROR", response);
    }
    if (!config.responseType) {
      return this.defaultDecoder(response);
    }
    let decoder;
    if (typeof config.responseType === "function") {
      decoder = config.responseType;
    } else {
      decoder = this._decoders[config.responseType];
      if (!decoder) {
        throw new TypeError(`Unknown responseType: ${config.responseType}`);
      }
    }
    return decoder(response);
  }
  async head(url, config) {
    const response = await this(url, { method: "HEAD", responseType: "headers", ...config });
    return this._decode(response);
  }
  ws(url, _config) {
    const config = this.resolveConfig(_config);
    url = this.resolveURL(url, config, true);
    const headers = new Headers(config.headers);
    const init = {
      headers
    };
    if (config.proxyAgent) {
      const proxyURL = new URL(config.proxyAgent);
      const factory = this._proxies[proxyURL.protocol.slice(0, -1)];
      if (!factory) throw new Error(`Cannot resolve proxy agent ${proxyURL}`);
      init.dispatcher = factory(proxyURL);
    }
    const socket = this.ctx.waterfall(this, "http/websocket", url, init, config, () => {
      this.ctx.logger("http:ws").debug("connect %s", url.href);
      return new this.undici.WebSocket(url, init);
    });
    const dispose = this.ctx.effect(() => {
      return () => socket.close(1e3, "context disposed");
    }, "new WebSocket()");
    socket.addEventListener("close", () => {
      dispose();
    });
    return socket;
  }
};
var index_default = Http;
export {
  Http,
  index_default as default
};
