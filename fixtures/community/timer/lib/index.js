var __defProp = Object.defineProperty;
var __name = (target, value) => __defProp(target, "name", { value, configurable: true });

// src/index.ts
import { Service } from "cordis";
var TimerService = class extends Service {
  static {
    __name(this, "TimerService");
  }
  constructor(ctx) {
    super(ctx, "timer");
    ctx.mixin("timer", ["timeout", "interval", "throttle", "debounce", "setTimeout", "setInterval"]);
  }
  /** @deprecated use `ctx.timeout()` instead */
  setTimeout(callback, delay) {
    return this.timeout(callback, delay);
  }
  /** @deprecated use `ctx.interval()` instead */
  setInterval(callback, delay) {
    return this.interval(callback, delay);
  }
  timeout(...args) {
    const callback = typeof args[0] === "function" ? args.shift() : void 0;
    const delay = args[0];
    if (callback) {
      const dispose = this.ctx.effect(() => {
        const timer = setTimeout(() => {
          dispose();
          callback();
        }, delay);
        return () => clearTimeout(timer);
      }, "ctx.timeout()");
      return dispose;
    } else {
      const { promise, resolve, reject } = Promise.withResolvers();
      const dispose = this.ctx.effect(() => {
        const timer = setTimeout(resolve, delay);
        return () => {
          clearTimeout(timer);
          reject(new Error("Context has been disposed"));
        };
      }, "ctx.timeout()");
      return promise.finally(dispose);
    }
  }
  interval(...args) {
    const callback = typeof args[0] === "function" ? args.shift() : void 0;
    const delay = args[0];
    if (callback) {
      return this.ctx.effect(() => {
        const timer = setInterval(callback, delay);
        return () => clearInterval(timer);
      }, "ctx.interval()");
    } else {
      let done;
      let nextTask;
      const dispose = this.ctx.effect(() => {
        const timer = setInterval(() => {
          nextTask?.resolve({ done: false, value: void 0 });
        }, delay);
        return () => {
          clearInterval(timer);
          if (done) return;
          done = { kind: "throw", reason: new Error("Context has been disposed") };
          nextTask?.reject(done.reason);
        };
      }, "ctx.interval()");
      return {
        next: /* @__PURE__ */ __name(() => {
          if (!done) return (nextTask = Promise.withResolvers()).promise;
          if (done.kind === "return") return Promise.resolve({ done: true, value: done.value });
          return Promise.reject(done.reason);
        }, "next"),
        return: /* @__PURE__ */ __name((value) => {
          if (!done) done = { kind: "return", value };
          nextTask?.resolve({ done: true, value });
          dispose();
          return Promise.resolve({ done: true, value });
        }, "return"),
        throw: /* @__PURE__ */ __name((reason) => {
          if (!done) done = { kind: "throw", reason };
          nextTask?.reject(reason);
          dispose();
          return Promise.resolve({ done: true, value: void 0 });
        }, "throw"),
        [Symbol.asyncIterator]() {
          return this;
        }
      };
    }
  }
  _schedule(label, trigger, isDisposed = false) {
    let timer;
    const dispose = this.ctx.effect(() => () => {
      isDisposed = true;
      clearTimeout(timer);
    }, label);
    const wrapper = /* @__PURE__ */ __name((...args) => {
      clearTimeout(timer);
      timer = trigger(args, isDisposed);
    }, "wrapper");
    wrapper.dispose = dispose;
    return wrapper;
  }
  throttle(callback, delay, noTrailing) {
    let lastCall = -Infinity;
    const execute = /* @__PURE__ */ __name((...args) => {
      lastCall = Date.now();
      callback(...args);
    }, "execute");
    return this._schedule("ctx.throttle()", (args, isDisposed) => {
      const now = Date.now();
      const remaining = delay - now + lastCall;
      if (remaining <= 0) {
        execute(...args);
      } else if (!isDisposed) {
        return setTimeout(execute, remaining, ...args);
      }
    }, noTrailing);
  }
  debounce(callback, delay) {
    return this._schedule("ctx.debounce()", (args, isDisposed) => {
      if (isDisposed) return;
      return setTimeout(callback, delay, ...args);
    });
  }
};
var index_default = TimerService;
export {
  TimerService,
  index_default as default
};
