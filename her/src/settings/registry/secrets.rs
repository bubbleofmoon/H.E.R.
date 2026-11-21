use crate::settings::{
    kinds::{AdministratorPasswordPhc, ApiKey},
    Setting,
};

pub(crate) const WEB_SEARCH_API_KEY: Setting = Setting {
    key: "webSearch.apiKey",
    pointer: "/webSearch/apiKey",
    kind: &ApiKey,
    applies: "live",
    group: "endpoint credentials",
    label: "Primary web search API key",
    hint:
        "Used for the current primary search endpoint. Set it again after changing that endpoint.",
    chat: false,
    secret: true,
};

pub(crate) const CLI_MODEL_API_KEY: Setting = Setting {
    key: "cliModel.apiKey",
    pointer: "/cliModel/apiKey",
    kind: &ApiKey,
    applies: "live",
    group: "endpoint credentials",
    label: "CLI model relay API key",
    hint:
        "Used for the selected runtime's model endpoint. Set it again after changing that endpoint.",
    chat: false,
    secret: true,
};

pub(crate) const ADMINISTRATOR_PASSWORD: Setting = Setting {
    key: "console.administratorPassword",
    pointer: "/console/administratorPassword",
    kind: &AdministratorPasswordPhc,
    applies: "live",
    group: "console authentication",
    label: "Console administrator password",
    hint: "Managed by console authentication.",
    chat: false,
    secret: true,
};

pub(super) const SETTINGS: &[Setting] = &[
    WEB_SEARCH_API_KEY,
    CLI_MODEL_API_KEY,
    ADMINISTRATOR_PASSWORD,
];
