package scim

import (
	"encoding/json"
	"errors"
	"fmt"
	"strings"
)

var (
	ErrMissingUserName = errors.New("scim: userName is required")
)

type Email struct {
	Value   string `json:"value"`
	Type    string `json:"type,omitempty"`
	Primary bool   `json:"primary,omitempty"`
}

type Name struct {
	GivenName  string `json:"givenName,omitempty"`
	FamilyName string `json:"familyName,omitempty"`
	Formatted  string `json:"formatted,omitempty"`
}

type ScimToken struct {
	Subject     string  `json:"sub,omitempty"`
	Issuer      string  `json:"iss,omitempty"`
	ExpiresAt   int64   `json:"exp,omitempty"`
	ExternalID  string  `json:"externalId,omitempty"`
	UserName    string  `json:"userName"`
	DisplayName string  `json:"displayName,omitempty"`
	Name        Name    `json:"name,omitempty"`
	Emails      []Email `json:"emails,omitempty"`
	Active      bool    `json:"active"`
}

func ParseScimToken(raw []byte) (*ScimToken, error) {
	var token ScimToken
	if err := json.Unmarshal(raw, &token); err != nil {
		return nil, fmt.Errorf("scim: decode token: %w", err)
	}
	if err := token.Validate(); err != nil {
		return nil, err
	}
	return &token, nil
}

func (t *ScimToken) Validate() error {
	if t == nil || strings.TrimSpace(t.UserName) == "" {
		return ErrMissingUserName
	}
	return nil
}

func (t *ScimToken) PrimaryEmail() string {
	for _, email := range t.Emails {
		if email.Primary && strings.TrimSpace(email.Value) != "" {
			return email.Value
		}
	}
	for _, email := range t.Emails {
		if strings.TrimSpace(email.Value) != "" {
			return email.Value
		}
	}
	return ""
}

func (t *ScimToken) MapToUserAttributes() (*User, error) {
	if err := t.Validate(); err != nil {
		return nil, err
	}
	displayName := strings.TrimSpace(t.DisplayName)
	if displayName == "" {
		displayName = strings.TrimSpace(t.Name.Formatted)
	}
	if displayName == "" {
		displayName = t.UserName
	}
	return &User{
		ExternalID:  t.ExternalID,
		UserName:    t.UserName,
		Email:       t.PrimaryEmail(),
		DisplayName: displayName,
		Active:      t.Active,
	}, nil
}
