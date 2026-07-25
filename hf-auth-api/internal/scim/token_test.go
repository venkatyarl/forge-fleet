package scim

import (
	"errors"
	"testing"
)

func TestParseScimTokenAndMapToUserAttributes(t *testing.T) {
	token, err := ParseScimToken([]byte(`{
		"sub":"subject-1",
		"externalId":"external-7",
		"userName":"ada",
		"name":{"formatted":"Ada Lovelace"},
		"emails":[
			{"value":"other@example.com"},
			{"value":"ada@example.com","primary":true}
		],
		"active":true
	}`))
	if err != nil {
		t.Fatalf("ParseScimToken() error = %v", err)
	}

	attributes, err := token.MapToUserAttributes()
	if err != nil {
		t.Fatalf("MapToUserAttributes() error = %v", err)
	}
	if attributes.ExternalID != "external-7" ||
		attributes.Username != "ada" ||
		attributes.Email != "ada@example.com" ||
		attributes.DisplayName != "Ada Lovelace" ||
		!attributes.Active {
		t.Errorf("unexpected mapped attributes: %+v", attributes)
	}
}

func TestMapToUserAttributesFallsBackToFirstEmailAndUserName(t *testing.T) {
	token := &ScimToken{
		UserName: "grace",
		Emails:   []Email{{Value: "grace@example.com"}},
	}

	attributes, err := token.MapToUserAttributes()
	if err != nil {
		t.Fatalf("MapToUserAttributes() error = %v", err)
	}
	if attributes.Email != "grace@example.com" {
		t.Errorf("Email = %q, want grace@example.com", attributes.Email)
	}
	if attributes.DisplayName != "grace" {
		t.Errorf("DisplayName = %q, want grace", attributes.DisplayName)
	}
	if attributes.Active {
		t.Error("Active = true, want false")
	}
}

func TestScimTokenValidation(t *testing.T) {
	tests := []struct {
		name  string
		token *ScimToken
		want  error
	}{
		{name: "nil token", token: nil, want: ErrMissingUserName},
		{name: "missing userName", token: &ScimToken{Emails: []Email{{Value: "a@example.com"}}}, want: ErrMissingUserName},
		{name: "missing emails", token: &ScimToken{UserName: "ada"}, want: ErrNoEmails},
		{name: "blank email", token: &ScimToken{UserName: "ada", Emails: []Email{{Value: "  "}}}, want: ErrNoEmails},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			if err := test.token.Validate(); !errors.Is(err, test.want) {
				t.Errorf("Validate() error = %v, want %v", err, test.want)
			}
		})
	}
}
