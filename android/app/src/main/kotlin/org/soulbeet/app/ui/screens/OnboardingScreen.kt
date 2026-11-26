package org.soulbeet.app.ui.screens

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import org.soulbeet.app.theme.LocalSoulbeetColors
import org.soulbeet.app.theme.LocalSoulbeetTypography
import org.soulbeet.app.theme.SoulbeetShapes
import org.soulbeet.app.ui.components.SoulbeetPixelLogo

@Composable
fun OnboardingScreen(
    initialServerUrl: String,
    isConnecting: Boolean,
    errorMessage: String?,
    onConnectPassword: (serverUrl: String, username: String, password: String) -> Unit,
    onConnectToken: (serverUrl: String, token: String, isBearer: Boolean) -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current

    var serverUrl by rememberSaveable { mutableStateOf(initialServerUrl) }
    var username by rememberSaveable { mutableStateOf("") }
    var password by rememberSaveable { mutableStateOf("") }
    var token by rememberSaveable { mutableStateOf("") }
    var authMode by rememberSaveable { mutableIntStateOf(0) } // 0 = Password, 1 = Session Token
    var tokenIsBearer by rememberSaveable { mutableStateOf(false) }

    val fieldColors = OutlinedTextFieldDefaults.colors(
        focusedBorderColor = colors.leafSolid,
        unfocusedBorderColor = colors.borderDefault,
        focusedTextColor = colors.textPrimary,
        unfocusedTextColor = colors.textPrimary,
        focusedContainerColor = colors.surfaceLow,
        unfocusedContainerColor = colors.surfaceLow,
        focusedLabelColor = colors.leafText,
        unfocusedLabelColor = colors.textMuted,
        cursorColor = colors.leafSolid,
    )

    Column(
        modifier = modifier
            .fillMaxSize()
            .background(colors.canvas)
            .statusBarsPadding()
            .navigationBarsPadding()
            .imePadding()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 24.dp, vertical = 32.dp),
        verticalArrangement = Arrangement.spacedBy(24.dp),
    ) {
        Spacer(modifier = Modifier.height(12.dp))

        Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
            SoulbeetPixelLogo(size = 64.dp)
            Text(
                text = "Connect to Soulbeet",
                style = type.display,
                color = colors.textPrimary,
            )
            Text(
                text = "Sync your Navidrome library, offline cache, and Discovery curation across Android and Android Auto.",
                style = type.bodySm,
                color = colors.textSecondary,
            )
        }

        // Auth Mode Segmented Selector
        Surface(
            color = colors.surfaceLow,
            shape = SoulbeetShapes.full,
            border = BorderStroke(1.dp, colors.borderSubtle),
        ) {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(4.dp),
                horizontalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                listOf("Account Sign-In", "Session Token").forEachIndexed { idx, label ->
                    val selected = authMode == idx
                    Box(
                        modifier = Modifier
                            .weight(1f)
                            .heightIn(min = 48.dp)
                            .background(
                                color = if (selected) colors.surfaceHigh else colors.surfaceLow,
                                shape = SoulbeetShapes.full,
                            )
                            .clickable { authMode = idx },
                        contentAlignment = Alignment.Center,
                    ) {
                        Text(
                            text = label,
                            style = type.bodySm,
                            color = if (selected) colors.textPrimary else colors.textMuted,
                        )
                    }
                }
            }
        }

        Surface(
            color = colors.surface,
            shape = SoulbeetShapes.lg,
            border = BorderStroke(1.dp, colors.borderSubtle),
        ) {
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(20.dp),
                verticalArrangement = Arrangement.spacedBy(16.dp),
            ) {
                OutlinedTextField(
                    value = serverUrl,
                    onValueChange = { serverUrl = it },
                    label = { Text("Soulbeet Server URL") },
                    placeholder = { Text("https://music.example.com", color = colors.textDisabled) },
                    singleLine = true,
                    colors = fieldColors,
                    shape = SoulbeetShapes.sm,
                    keyboardOptions = KeyboardOptions(
                        keyboardType = KeyboardType.Uri,
                        imeAction = ImeAction.Next,
                    ),
                    modifier = Modifier.fillMaxWidth(),
                )

                if (authMode == 0) {
                    OutlinedTextField(
                        value = username,
                        onValueChange = { username = it },
                        label = { Text("Username") },
                        singleLine = true,
                        colors = fieldColors,
                        shape = SoulbeetShapes.sm,
                        keyboardOptions = KeyboardOptions(imeAction = ImeAction.Next),
                        modifier = Modifier.fillMaxWidth(),
                    )
                    OutlinedTextField(
                        value = password,
                        onValueChange = { password = it },
                        label = { Text("Password") },
                        singleLine = true,
                        visualTransformation = PasswordVisualTransformation(),
                        colors = fieldColors,
                        shape = SoulbeetShapes.sm,
                        keyboardOptions = KeyboardOptions(
                            keyboardType = KeyboardType.Password,
                            imeAction = ImeAction.Done,
                        ),
                        keyboardActions = KeyboardActions(
                            onDone = { onConnectPassword(serverUrl, username, password) },
                        ),
                        modifier = Modifier.fillMaxWidth(),
                    )
                } else {
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        listOf(false to "Cookie (auth_token)", true to "Bearer Token").forEach { (isBearerOpt, label) ->
                            val selected = tokenIsBearer == isBearerOpt
                            Surface(
                                color = if (selected) colors.leafSurface else colors.surfaceLow,
                                shape = SoulbeetShapes.full,
                                border = BorderStroke(
                                    1.dp,
                                    if (selected) colors.leafText.copy(alpha = 0.5f) else colors.borderSubtle,
                                ),
                                modifier = Modifier
                                    .weight(1f)
                                    .heightIn(min = 48.dp)
                                    .clickable { tokenIsBearer = isBearerOpt },
                            ) {
                                Box(contentAlignment = Alignment.Center, modifier = Modifier.padding(horizontal = 10.dp)) {
                                    Text(
                                        text = label,
                                        style = type.monoXs,
                                        color = if (selected) colors.leafText else colors.textSecondary,
                                    )
                                }
                            }
                        }
                    }

                    OutlinedTextField(
                        value = token,
                        onValueChange = { token = it },
                        label = {
                            Text(if (tokenIsBearer) "Bearer Token" else "Session Cookie (auth_token)")
                        },
                        placeholder = {
                            Text(
                                if (tokenIsBearer) "Paste Bearer token" else "Paste auth_token value",
                                color = colors.textDisabled,
                            )
                        },
                        singleLine = true,
                        colors = fieldColors,
                        shape = SoulbeetShapes.sm,
                        keyboardOptions = KeyboardOptions(imeAction = ImeAction.Done),
                        keyboardActions = KeyboardActions(
                            onDone = { onConnectToken(serverUrl, token, tokenIsBearer) },
                        ),
                        modifier = Modifier.fillMaxWidth(),
                    )
                }

                if (!errorMessage.isNullOrBlank()) {
                    Surface(
                        color = colors.crimsonSurface,
                        shape = SoulbeetShapes.sm,
                    ) {
                        Text(
                            text = errorMessage,
                            style = type.bodySm,
                            color = colors.crimsonText,
                            modifier = Modifier
                                .fillMaxWidth()
                                .padding(12.dp),
                        )
                    }
                }

                Button(
                    onClick = {
                        if (authMode == 0) {
                            onConnectPassword(serverUrl, username, password)
                        } else {
                            onConnectToken(serverUrl, token, tokenIsBearer)
                        }
                    },
                    enabled = !isConnecting,
                    colors = ButtonDefaults.buttonColors(
                        containerColor = colors.leafSolid,
                        contentColor = colors.leafInk,
                    ),
                    shape = SoulbeetShapes.full,
                    modifier = Modifier
                        .fillMaxWidth()
                        .heightIn(min = 52.dp),
                ) {
                    if (isConnecting) {
                        CircularProgressIndicator(
                            modifier = Modifier.size(20.dp),
                            strokeWidth = 2.dp,
                            color = colors.leafInk,
                        )
                    } else {
                        Text(text = "Connect & Sync Library", style = type.body)
                    }
                }
            }
        }
    }
}
