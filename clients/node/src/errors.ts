/** Errors thrown by the tikovm client. */
export class TikovmError extends Error {}

/**
 * hostd responded with a non-2xx status. `status` is the HTTP status code
 * and `code` is the `error.code` from the uniform API error body
 * (hostd/src/api/error.rs); the two are currently identical.
 */
export class TikovmApiError extends TikovmError {
  readonly status: number;
  readonly code: number;

  constructor(status: number, code: number, message: string) {
    super(message);
    this.name = 'TikovmApiError';
    this.status = status;
    this.code = code;
  }
}

/** The HTTP request failed at the transport level (e.g. connection refused). */
export class TikovmRequestError extends TikovmError {
  readonly url: string;

  constructor(url: string, message: string, cause: unknown) {
    super(message, { cause });
    this.name = 'TikovmRequestError';
    this.url = url;
  }
}

/** The response could not be decoded as JSON or is missing the error body. */
export class TikovmProtocolError extends TikovmError {}
