import type {
  CreateElicitationResponse,
  ElicitationPropertySchema,
  ElicitationSchema,
  EnumOption,
} from "@agentclientprotocol/sdk"
import type { OpenCodeClient } from "@opencode/client/effect"
import { Form } from "@opencode/schema/form"
import { Effect, Option, Schema } from "effect"
import type { Capabilities } from "./capabilities"
import { ACPChild } from "./child"
import { ACPClient } from "./client"
import type { ACPConnection } from "./connection"

export type AskedForm = Omit<Form.Info, "id"> & { readonly id: string }
type InputField = Exclude<Form.Field, Form.ExternalField>
type SelectField = Form.StringField | Form.MultiselectField

/**
 * The client-side extension the grok-derived TUI already implements
 * (`x.ai/ask_user_question`). ACP's Rust client decodes any `_`-prefixed method
 * as an extension request and strips the prefix, so the kernel sends the
 * underscore form and the pager dispatches the bare name.
 */
const QuestionExtMethod = "_x.ai/ask_user_question"

/** Matches the TUI's own 30-minute budget for a whole questionnaire. */
const QuestionExtTimeout = "30 minutes" as const

/** One question as the TUI's `x.ai/ask_user_question` request expects it. */
type ExtQuestion = {
  readonly question: string
  readonly options: Array<{ readonly label: string; readonly description: string }>
  readonly multi_select: boolean
}

/** The subset of `AskUserQuestionExtResponse` this module reads. */
const ExtResponse = Schema.Struct({
  outcome: Schema.String,
  answers: Schema.optional(Schema.Record(Schema.String, Schema.Array(Schema.String))),
  annotations: Schema.optional(
    Schema.Record(Schema.String, Schema.Struct({ notes: Schema.optional(Schema.String) })),
  ),
})

const QuestionKind = "question"
// Form mode must not collect secrets; elicit only flows known to be credential-free.
const ElicitedKind = Schema.Struct({ kind: Schema.Literals([QuestionKind, "websearch.provider"]) })
const Credential = /password|passphrase|secret|token|api[_ -]?key|credential|private[_ -]?key/i
const ToolSource = Schema.Struct({ tool: Schema.Struct({ id: Schema.String }) })

type Input = {
  readonly client: OpenCodeClient
  readonly connection: ACPConnection.Interface
  readonly form: Form.Info
  /** Only the elicitation path needs a schema; the extension path maps fields itself. */
  readonly requestedSchema?: ElicitationSchema
  readonly clientSessionID: string
  readonly child?: ACPChild.Session
  readonly toolCallSent: boolean
}

export const UnshownQuestionMessage =
  "The question couldn't be shown to the user in this client. Continue without an answer: make reasonable assumptions and state them, or ask the user in your reply if you can't proceed."

/**
 * Sent when the user really did cancel. The `question` tool only continues the
 * turn when the cancellation carries a message; without one it raises
 * `CancelledError`, which the runner turns into a `STEP_INTERRUPTED` decline.
 */
export const CancelledQuestionMessage =
  "The user dismissed the question. Continue with reasonable assumptions and state them, or ask again in your reply if you cannot proceed."

function cancel(client: OpenCodeClient, form: Form.Info, message?: string) {
  return client.session.form.cancel({ sessionID: form.sessionID, formID: form.id, message }).pipe(
    Effect.catchTag(["FormAlreadySettledError", "FormNotFoundError"], () => Effect.void),
    Effect.catch(() =>
      ACPClient.decodeSessionID(form.sessionID).pipe(
        Effect.flatMap((sessionID) => client.session.interrupt({ sessionID })),
        Effect.ignore,
      ),
    ),
  )
}

export function requestedSchema(form: AskedForm, capabilities: Capabilities): ElicitationSchema | undefined {
  if (!capabilities.formElicitation) return undefined
  if (Option.isNone(Schema.decodeUnknownOption(ElicitedKind)(form.metadata))) return undefined
  if (form.fields.some(credentialLike)) return undefined
  const fields = form.fields.filter((field): field is InputField => field.type !== "external")
  if (fields.length !== form.fields.length || fields.some((field) => field.when?.length)) return undefined
  if (fields.some((field) => field.hidden && field.required && field.default === undefined)) return undefined
  const keys = new Set(fields.map((field) => field.key))
  const visible = fields.filter((field) => !field.hidden)
  if (!visible.every((field) => representable(field, keys))) return undefined
  return {
    type: "object",
    properties: Object.fromEntries(visible.flatMap(properties)),
    required: visible.filter((field) => field.required).map((field) => field.key),
  }
}

// The question tool returns the message to the model, so the turn continues instead of ending as interrupted.
export function cancelUnshown(client: OpenCodeClient, form: Form.Info) {
  return cancel(client, form, form.metadata?.kind === QuestionKind ? UnshownQuestionMessage : undefined).pipe(
    Effect.uninterruptible,
  )
}

function answer(form: AskedForm, response: CreateElicitationResponse): Form.Answer | undefined {
  if (response.action !== "accept") return undefined
  const content = Schema.decodeUnknownOption(Form.Answer)(response.content ?? {})
  if (Option.isNone(content)) return undefined
  return Object.fromEntries(
    form.fields.flatMap((field) => {
      const value = fieldAnswer(field, content.value)
      return value === undefined ? [] : [[field.key, value]]
    }),
  )
}

export const ask = Effect.fnUntraced(function* (input: Input) {
  const source = input.toolCallSent ? Schema.decodeUnknownOption(ToolSource)(input.form.metadata) : Option.none()
  const toolCallID = Option.getOrUndefined(Option.map(source, (metadata) => metadata.tool.id))
  const response = yield* input.connection.createElicitation({
    mode: "form",
    sessionId: input.clientSessionID,
    ...(toolCallID ? { toolCallId: ACPChild.toolCallID(input.child, toolCallID) } : {}),
    message: ACPChild.prefixTitle(input.child, input.form.title),
    requestedSchema: input.requestedSchema,
  })
  return answer(input.form, response) ?? "cancel"
})

/**
 * Ask a `question` form over the client's `x.ai/ask_user_question` extension.
 *
 * This is the channel the grok-derived TUI actually implements: it owns the
 * overlay, multi-select, free-form input and child-session routing, so the
 * kernel only maps fields in and answers back. Returns `"unshown"` when the form
 * cannot be represented, which the caller settles through `cancelUnshown`.
 */
export const askExt = Effect.fnUntraced(function* (input: Input) {
  const mapped = extQuestions(input.form)
  if (mapped === undefined) return "unshown" as const
  const source = input.toolCallSent ? Schema.decodeUnknownOption(ToolSource)(input.form.metadata) : Option.none()
  const toolCallID = Option.getOrUndefined(Option.map(source, (metadata) => metadata.tool.id))
  const response = yield* input.connection.extRequest(QuestionExtMethod, {
    sessionId: input.clientSessionID,
    // The TUI requires a non-empty toolCallId, but the kernel has none in
    // background/child flows; the form id is stable for the same purpose.
    toolCallId: ACPChild.toolCallID(input.child, toolCallID ?? input.form.id),
    questions: mapped.questions,
    mode: "default",
  }).pipe(Effect.timeoutOption(QuestionExtTimeout))
  // The pager keeps an unattached question pending until its session attaches,
  // so a client that never answers must not hold the turn forever.
  if (Option.isNone(response)) return "unshown" as const
  return extAnswer(input.form, mapped, response.value) ?? "cancel"
})

/**
 * Whether the question extension can carry this form.
 *
 * The TUI only renders option lists plus a free-form row, so a form with
 * numbers, booleans or credential-looking fields keeps the elicitation path
 * (or `cancelUnshown`) instead of being silently mangled.
 */
export function extQuestions(
  form: AskedForm,
): { readonly questions: ExtQuestion[]; readonly keys: ReadonlyMap<string, string> } | undefined {
  if (form.metadata?.kind !== QuestionKind) return undefined
  if (form.fields.some((field) => field.type === "external")) return undefined
  if (form.fields.some(credentialLike)) return undefined
  const questions: ExtQuestion[] = []
  const keys = new Map<string, string>()
  for (const field of form.fields) {
    if (field.type !== "string" && field.type !== "multiselect") return undefined
    const text = dedupe(questions, field.description ?? field.title ?? field.key)
    // `title` has no slot in the TUI's Question and would pollute the answer
    // key, so it stays out of the wire payload.
    questions.push({
      question: text,
      options: (field.options ?? []).map((option) => ({
        label: option.label,
        description: option.description ?? "",
      })),
      multi_select: field.type === "multiselect",
    })
    keys.set(text, field.key)
  }
  return { questions, keys }
}

function dedupe(questions: ReadonlyArray<ExtQuestion>, text: string) {
  const taken = new Set(questions.map((question) => question.question))
  if (!taken.has(text)) return text
  let suffix = 2
  while (taken.has(`${text} (${suffix})`)) suffix++
  return `${text} (${suffix})`
}

function extAnswer(
  form: AskedForm,
  mapped: NonNullable<ReturnType<typeof extQuestions>>,
  response: unknown,
): Form.Answer | undefined {
  const decoded = Schema.decodeUnknownOption(ExtResponse)(response)
  if (Option.isNone(decoded) || decoded.value.outcome !== "accepted") return undefined
  const { answers = {}, annotations = {} } = decoded.value
  return Object.fromEntries(
    [...mapped.keys].flatMap(([text, key]) => {
      const labels = answers[text] ?? []
      const field = form.fields.find((item) => item.key === key)
      // A free-form-only answer arrives as the literal label "Other"; the typed
      // text lives in `annotations[question].notes` and is the real answer.
      const notes = annotations[text]?.notes
      if (notes !== undefined && notes.trim() !== "") {
        if (field?.type === "multiselect") return [[key, [...labels.filter((label) => label !== "Other"), notes]]]
        return [[key, notes]]
      }
      if (labels.length === 0) return []
      if (field?.type === "multiselect") return [[key, labels]]
      return [[key, labels[0]!]]
    }),
  )
}

export function respond(input: Input, outcome: AskOutcome) {
  return settle(input.client, input.form, outcome)
}
/** A question form outcome, including the "this client cannot show it" case. */
export type AskOutcome = Form.Answer | "cancel" | "unshown"

/**
 * Settle a form for every non-answer outcome.
 *
 * `question` only continues the turn when a cancellation carries a message; a
 * bare cancellation raises `CancelledError`, which the runner turns into a
 * `STEP_INTERRUPTED` decline.
 */
function settle(client: OpenCodeClient, form: Form.Info, outcome: AskOutcome) {
  if (outcome === "unshown") return cancelUnshown(client, form)
  if (outcome === "cancel") return cancel(client, form, CancelledQuestionMessage)
  return client.session.form
    .reply({ sessionID: form.sessionID, formID: form.id, answer: outcome })
    .pipe(
      Effect.catchTag(["FormAlreadySettledError", "FormNotFoundError"], () => Effect.void),
      Effect.catch((cause) =>
        Effect.logWarning("ACP form reply failed", cause).pipe(Effect.andThen(cancel(client, form))),
      ),
    )
}

function credentialLike(field: Form.Field) {
  const labels =
    field.type === "string" || field.type === "multiselect" ? (field.options ?? []).map((option) => option.label) : []
  return [field.key, field.title, field.description, ...labels].some(
    (text) => text !== undefined && Credential.test(text),
  )
}

function representable(field: InputField, keys: ReadonlySet<string>) {
  if (field.type !== "string" && field.type !== "multiselect") return true
  if (!hasOptions(field)) return true
  const values = new Set(field.options?.map((option) => option.value))
  const defaults =
    field.default === undefined ? [] : typeof field.default === "string" ? [field.default] : field.default
  if (defaults.some((value) => !values.has(value))) return false
  if (!field.custom) return true
  if (field.required || keys.has(customKey(field))) return false
  return field.type === "string" || (field.minItems === undefined && field.maxItems === undefined)
}

function properties(field: InputField): Array<[string, ElicitationPropertySchema]> {
  const base = { title: field.title, description: field.description }
  switch (field.type) {
    case "string": {
      if (!hasOptions(field))
        return [[field.key, { type: "string", ...base, ...stringConstraints(field), default: field.default }]]
      const select: ElicitationPropertySchema = {
        type: "string",
        ...base,
        oneOf: options(field),
        default: field.default,
      }
      return field.custom ? [[field.key, select], other(field, "Type your own answer")] : [[field.key, select]]
    }
    case "multiselect": {
      const select: ElicitationPropertySchema = {
        type: "array",
        ...base,
        items: { anyOf: options(field) },
        minItems: field.required ? Math.max(field.minItems ?? 0, 1) : field.minItems,
        maxItems: field.maxItems,
        default: field.default,
      }
      return field.custom ? [[field.key, select], other(field, "Add your own answer")] : [[field.key, select]]
    }
    case "number":
    case "integer":
      return [
        [
          field.key,
          { type: field.type, ...base, minimum: field.minimum, maximum: field.maximum, default: field.default },
        ],
      ]
    case "boolean":
      return [[field.key, { type: "boolean", ...base, default: field.default }]]
  }
}

function other(field: SelectField, description: string): [string, ElicitationPropertySchema] {
  return [
    customKey(field),
    {
      type: "string",
      title: `${field.title ?? field.key} (other)`,
      description,
      ...(field.type === "string" ? stringConstraints(field) : {}),
    },
  ]
}

// Core rejects an empty string for a required field.
function stringConstraints(field: Form.StringField) {
  return {
    format: field.format,
    minLength: field.required ? Math.max(field.minLength ?? 0, 1) : field.minLength,
    maxLength: field.maxLength,
    pattern: field.pattern,
  }
}

function options(field: SelectField): EnumOption[] {
  return (field.options ?? []).map((option) => ({
    const: option.value,
    title: option.label,
    description: option.description,
  }))
}

function fieldAnswer(field: Form.Field, content: Form.Answer) {
  if (field.type !== "external" && field.hidden) return field.default
  const value = content[field.key]
  if ((field.type !== "string" && field.type !== "multiselect") || !field.custom || !hasOptions(field)) return value
  const custom = content[customKey(field)]
  if (typeof custom !== "string" || custom.trim() === "") return value
  if (field.type === "string") return custom
  return Array.isArray(value) ? [...value, custom] : [custom]
}

function hasOptions(field: SelectField) {
  return field.type === "multiselect" || field.options !== undefined
}

function customKey(field: SelectField) {
  return `${field.key}_custom`
}

export * as ACPElicitation from "./elicitation"
