//! OTLP tracing when built with `--features otlp`.
//!
//! Set `OTEL_EXPORTER_OTLP_ENDPOINT` (for example `http://127.0.0.1:4317`).

pub fn init_otlp() -> anyhow::Result<()> {
    use opentelemetry::global;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt;

    global::set_text_map_propagator(TraceContextPropagator::new());

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()?;

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .build();

    global::set_tracer_provider(provider.clone());
    let tracer = provider.tracer("mcp-forge");

    let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = Registry::default()
        .with(telemetry)
        .with(tracing_subscriber::fmt::layer());
    tracing::subscriber::set_global_default(subscriber)?;
    Ok(())
}
