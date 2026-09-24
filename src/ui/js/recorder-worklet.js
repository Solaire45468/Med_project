class PCMRecorderProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.port.onmessage = (event) => {
      if (event.data && event.data.type === "drain") {
        this.port.postMessage({ type: "drained" });
      }
    };
  }

  process(inputs) {
    const input = inputs[0];
    if (!input || !input[0]) return true;

    const channel = input[0];
    this.port.postMessage(channel.slice(0));
    return true;
  }
}

registerProcessor("pcm-recorder", PCMRecorderProcessor);
